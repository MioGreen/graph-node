use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use graph::data::subgraph::schema::*;
use graph::prelude::{
    CreateSubgraphResult, SubgraphAssignmentProvider as SubgraphAssignmentProviderTrait,
    SubgraphRegistrar as SubgraphRegistrarTrait, *,
};

pub struct SubgraphRegistrar<L, P, S, CS> {
    logger: Logger,
    resolver: Arc<L>,
    provider: Arc<P>,
    store: Arc<S>,
    chain_store: Arc<CS>,
    node_id: NodeId,
    assignment_event_stream_cancel_guard: CancelGuard, // cancels on drop
}

impl<L, P, S, CS> SubgraphRegistrar<L, P, S, CS>
where
    L: LinkResolver,
    P: SubgraphAssignmentProviderTrait,
    S: Store,
    CS: ChainStore,
{
    pub fn new(
        logger: Logger,
        resolver: Arc<L>,
        provider: Arc<P>,
        store: Arc<S>,
        chain_store: Arc<CS>,
        node_id: NodeId,
    ) -> Self {
        let logger = logger.new(o!("component" => "SubgraphRegistrar"));

        SubgraphRegistrar {
            logger,
            resolver,
            provider,
            store,
            chain_store,
            node_id,
            assignment_event_stream_cancel_guard: CancelGuard::new(),
        }
    }

    pub fn start(&self) -> impl Future<Item = (), Error = Error> {
        let logger_clone1 = self.logger.clone();
        let logger_clone2 = self.logger.clone();
        let provider = self.provider.clone();
        let node_id = self.node_id.clone();
        let assignment_event_stream_cancel_handle =
            self.assignment_event_stream_cancel_guard.handle();

        // The order of the following three steps is important:
        // - Start assignment event stream
        // - Read assignments table and start assigned subgraphs
        // - Start processing assignment event stream
        //
        // Starting the event stream before reading the assignments table ensures that no
        // assignments are missed in the period of time between the table read and starting event
        // processing.
        // Delaying the start of event processing until after the table has been read and processed
        // ensures that Remove events happen after the assigned subgraphs have been started, not
        // before (otherwise a subgraph could be left running due to a race condition).
        //
        // The discrepancy between the start time of the event stream and the table read can result
        // in some extraneous events on start up. Examples:
        // - The event stream sees an Add event for subgraph A, but the table query finds that
        //   subgraph A is already in the table.
        // - The event stream sees a Remove event for subgraph B, but the table query finds that
        //   subgraph B has already been removed.
        // The `handle_assignment_events` function handles these cases by ignoring AlreadyRunning
        // (on subgraph start) or NotRunning (on subgraph stop) error types, which makes the
        // operations idempotent.

        // Start event stream
        let assignment_event_stream = self.assignment_events();

        // Deploy named subgraphs found in store
        self.start_assigned_subgraphs().and_then(move |()| {
            // Spawn a task to handle assignment events
            tokio::spawn(future::lazy(move || {
                assignment_event_stream
                    .map_err(SubgraphAssignmentProviderError::Unknown)
                    .map_err(CancelableError::Error)
                    .cancelable(&assignment_event_stream_cancel_handle, || {
                        CancelableError::Cancel
                    })
                    .for_each(move |assignment_event| {
                        assert_eq!(assignment_event.node_id(), &node_id);
                        handle_assignment_event(assignment_event, provider.clone(), &logger_clone1)
                    })
                    .map_err(move |e| match e {
                        CancelableError::Cancel => {}
                        CancelableError::Error(e) => {
                            error!(logger_clone2, "assignment event stream failed: {}", e);
                            panic!("assignment event stream error: {}", e);
                        }
                    })
            }));

            Ok(())
        })
    }

    pub fn assignment_events(&self) -> impl Stream<Item = AssignmentEvent, Error = Error> + Send {
        let store = self.store.clone();
        let node_id = self.node_id.clone();

        store
            .subscribe(vec![
                SubgraphDeploymentAssignmentEntity::subgraph_entity_pair(),
            ])
            .map_err(|()| format_err!("Entity change stream failed"))
            .and_then(
                move |entity_change| -> Result<Box<Stream<Item = _, Error = _> + Send>, _> {
                    let subgraph_hash = SubgraphDeploymentId::new(entity_change.entity_id.clone())
                        .map_err(|()| format_err!("Invalid subgraph hash in assignment entity"))?;

                    match entity_change.operation {
                        EntityChangeOperation::Added | EntityChangeOperation::Updated => {
                            store
                                .get(SubgraphDeploymentAssignmentEntity::key(
                                    subgraph_hash.clone(),
                                ))
                                .map_err(|e| {
                                    format_err!("Failed to get subgraph assignment entity: {}", e)
                                })
                                .map(|entity_opt| -> Box<Stream<Item = _, Error = _> + Send> {
                                    if let Some(entity) = entity_opt {
                                        if entity.get("nodeId") == Some(&node_id.to_string().into())
                                        {
                                            // Start subgraph on this node
                                            Box::new(stream::once(Ok(AssignmentEvent::Add {
                                                subgraph_id: subgraph_hash,
                                                node_id: node_id.clone(),
                                            })))
                                        } else {
                                            // Ensure it is removed from this node
                                            Box::new(stream::once(Ok(AssignmentEvent::Remove {
                                                subgraph_id: subgraph_hash,
                                                node_id: node_id.clone(),
                                            })))
                                        }
                                    } else {
                                        // Was added/updated, but is now gone.
                                        // We will get a separate Removed event later.
                                        Box::new(stream::empty())
                                    }
                                })
                        }
                        EntityChangeOperation::Removed => {
                            // Send remove event without checking node ID.
                            // If node ID does not match, then this is a no-op when handled in
                            // assignment provider.
                            Ok(Box::new(stream::once(Ok(AssignmentEvent::Remove {
                                subgraph_id: subgraph_hash,
                                node_id: node_id.clone(),
                            }))))
                        }
                    }
                },
            )
            .flatten()
    }

    fn start_assigned_subgraphs(&self) -> impl Future<Item = (), Error = Error> {
        let provider = self.provider.clone();

        // Create a query to find all assignments with this node ID
        let assignment_query = SubgraphDeploymentAssignmentEntity::query().filter(
            EntityFilter::Equal("nodeId".to_owned(), self.node_id.to_string().into()),
        );

        future::result(self.store.find(assignment_query))
            .map_err(|e| format_err!("Error querying subgraph assignments: {}", e))
            .and_then(move |assignment_entities| {
                assignment_entities
                    .into_iter()
                    .map(|assignment_entity| {
                        // Parse as subgraph hash
                        assignment_entity.id().and_then(|id| {
                            SubgraphDeploymentId::new(id).map_err(|()| {
                                format_err!("Invalid subgraph hash in assignment entity")
                            })
                        })
                    })
                    .collect::<Result<HashSet<SubgraphDeploymentId>, _>>()
            })
            .and_then(move |subgraph_ids| {
                let provider = provider.clone();
                stream::iter_ok(subgraph_ids).for_each(move |id| provider.start(id).from_err())
            })
    }
}

impl<L, P, S, CS> SubgraphRegistrarTrait for SubgraphRegistrar<L, P, S, CS>
where
    L: LinkResolver,
    P: SubgraphAssignmentProviderTrait,
    S: Store,
    CS: ChainStore,
{
    fn create_subgraph(
        &self,
        name: SubgraphName,
    ) -> Box<Future<Item = CreateSubgraphResult, Error = SubgraphRegistrarError> + Send + 'static>
    {
        Box::new(future::result(create_subgraph(
            &self.logger,
            self.store.clone(),
            name,
        )))
    }

    fn create_subgraph_version(
        &self,
        name: SubgraphName,
        hash: SubgraphDeploymentId,
        node_id: NodeId,
    ) -> Box<Future<Item = (), Error = SubgraphRegistrarError> + Send + 'static> {
        let logger = self.logger.clone();
        let store = self.store.clone();
        let chain_store = self.chain_store.clone();

        Box::new(
            SubgraphManifest::resolve(hash.to_ipfs_link(), self.resolver.clone())
                .map_err(SubgraphRegistrarError::ResolveError)
                .and_then(move |manifest| {
                    create_subgraph_version(&logger, store, chain_store, name, manifest, node_id)
                }),
        )
    }

    fn remove_subgraph(
        &self,
        name: SubgraphName,
    ) -> Box<Future<Item = (), Error = SubgraphRegistrarError> + Send + 'static> {
        Box::new(future::result(remove_subgraph(
            &self.logger,
            self.store.clone(),
            name,
        )))
    }

    fn list_subgraphs(
        &self,
    ) -> Box<Future<Item = Vec<SubgraphName>, Error = SubgraphRegistrarError> + Send + 'static>
    {
        Box::new(
            future::result(self.store.find(SubgraphEntity::query()))
                .from_err()
                .and_then(|subgraph_entities| {
                    subgraph_entities
                        .into_iter()
                        .map(|mut entity| {
                            let name_string = entity.remove("name").unwrap().as_string().unwrap();
                            SubgraphName::new(name_string.to_owned())
                                .map_err(|()| {
                                    format_err!(
                                        "Subgraph name in store has invalid format: {:?}",
                                        name_string
                                    )
                                })
                                .map_err(SubgraphRegistrarError::from)
                        })
                        .collect::<Result<_, _>>()
                }),
        )
    }
}

fn handle_assignment_event<P>(
    event: AssignmentEvent,
    provider: Arc<P>,
    logger: &Logger,
) -> Box<Future<Item = (), Error = CancelableError<SubgraphAssignmentProviderError>> + Send>
where
    P: SubgraphAssignmentProviderTrait,
{
    let logger = logger.to_owned();

    debug!(logger, "Received assignment event: {:?}", event);

    match event {
        AssignmentEvent::Add {
            subgraph_id,
            node_id: _,
        } => {
            Box::new(
                provider
                    .start(subgraph_id.clone())
                    .then(move |result| -> Result<(), _> {
                        match result {
                            Ok(()) => Ok(()),
                            Err(SubgraphAssignmentProviderError::AlreadyRunning(_)) => Ok(()),
                            Err(e) => {
                                // Errors here are likely an issue with the subgraph.
                                // These will be recorded eventually so that they can be displayed
                                // in a UI.
                                error!(
                                    logger,
                                    "Subgraph instance failed to start";
                                    "error" => e.to_string(),
                                    "subgraph_id" => subgraph_id.to_string()
                                );
                                Ok(())
                            }
                        }
                    }),
            )
        }
        AssignmentEvent::Remove {
            subgraph_id,
            node_id: _,
        } => Box::new(
            provider
                .stop(subgraph_id)
                .then(|result| match result {
                    Ok(()) => Ok(()),
                    Err(SubgraphAssignmentProviderError::NotRunning(_)) => Ok(()),
                    Err(e) => Err(e),
                })
                .map_err(CancelableError::Error),
        ),
    }
}

fn create_subgraph(
    logger: &Logger,
    store: Arc<impl Store>,
    name: SubgraphName,
) -> Result<CreateSubgraphResult, SubgraphRegistrarError> {
    let mut ops = vec![];

    // Check if this subgraph already exists
    let subgraph_entity_opt = store.find_one(SubgraphEntity::query().filter(
        EntityFilter::Equal("name".to_owned(), name.to_string().into()),
    ))?;
    if subgraph_entity_opt.is_some() {
        debug!(
            logger,
            "Subgraph name already exists: {:?}",
            name.to_string()
        );
        return Err(SubgraphRegistrarError::NameExists(name.to_string()));
    }

    ops.push(EntityOperation::AbortUnless {
        description: "Subgraph entity should not exist".to_owned(),
        query: SubgraphEntity::query().filter(EntityFilter::Equal(
            "name".to_owned(),
            name.to_string().into(),
        )),
        entity_ids: vec![],
    });

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let entity = SubgraphEntity::new(name.clone(), None, created_at);
    let entity_id = generate_entity_id();
    ops.extend(entity.write_operations(&entity_id));

    store.apply_entity_operations(ops, EventSource::None)?;

    debug!(logger, "Created subgraph"; "subgraph_name" => name.to_string());

    Ok(CreateSubgraphResult { id: entity_id })
}

fn create_subgraph_version(
    logger: &Logger,
    store: Arc<impl Store>,
    chain_store: Arc<impl ChainStore>,
    name: SubgraphName,
    manifest: SubgraphManifest,
    node_id: NodeId,
) -> Result<(), SubgraphRegistrarError> {
    let mut ops = vec![];

    // Look up subgraph entity by name
    let subgraph_entity_opt = store.find_one(SubgraphEntity::query().filter(
        EntityFilter::Equal("name".to_owned(), name.to_string().into()),
    ))?;
    let subgraph_entity = subgraph_entity_opt.ok_or_else(|| {
        debug!(
            logger,
            "Subgraph not found, could not create_subgraph_version";
            "subgraph_name" => name.to_string()
        );
        SubgraphRegistrarError::NameNotFound(name.to_string())
    })?;
    let subgraph_entity_id = subgraph_entity.id()?;
    let current_version_id_opt = match subgraph_entity.get("currentVersion") {
        Some(Value::String(current_version_id)) => Some(current_version_id.to_owned()),
        Some(Value::Null) => None,
        None => None,
        Some(_) => panic!("subgraph entity has invalid type in currentVersion field"),
    };
    ops.push(EntityOperation::AbortUnless {
        description: "Subgraph entity must still exist, have same name and currentVersion"
            .to_owned(),
        query: SubgraphEntity::query().filter(EntityFilter::And(vec![
            EntityFilter::Equal("name".to_owned(), name.to_string().into()),
            EntityFilter::Equal(
                "currentVersion".to_owned(),
                current_version_id_opt
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            ),
        ])),
        entity_ids: vec![subgraph_entity_id.clone()],
    });

    // Create the subgraph version entity
    let version_entity_id = generate_entity_id();
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    ops.extend(
        SubgraphVersionEntity::new(subgraph_entity_id.clone(), manifest.id.clone(), created_at)
            .write_operations(&version_entity_id),
    );

    // Check if subgraph deployment already exists for this hash
    let deployment_entity_opt = store.get(SubgraphDeploymentEntity::key(manifest.id.clone()))?;
    ops.push(EntityOperation::AbortUnless {
        description: "Subgraph deployment entity must continue to exist/not exist".to_owned(),
        query: SubgraphDeploymentEntity::query().filter(EntityFilter::Equal(
            "id".to_owned(),
            manifest.id.to_string().into(),
        )),
        entity_ids: if deployment_entity_opt.is_some() {
            vec![manifest.id.to_string()]
        } else {
            vec![]
        },
    });

    // Create deployment only if it does not exist already
    if deployment_entity_opt.is_none() {
        let chain_head_ptr_opt = chain_store.chain_head_ptr()?;
        let chain_head_block_number = match chain_head_ptr_opt {
            Some(chain_head_ptr) => chain_head_ptr.number,
            None => 0,
        };
        let genesis_block_ptr = chain_store.genesis_block_ptr()?;
        ops.extend(
            SubgraphDeploymentEntity::new(
                &manifest,
                false,
                false,
                genesis_block_ptr,
                chain_head_block_number,
            )
            .create_operations(&manifest.id),
        );
    }

    // If currentVersion is actually being changed, an old assignment may need to be removed.
    if current_version_id_opt != Some(manifest.id.to_string()) {
        // If there is a previous version that will no longer be "current"
        if let Some(current_version_id) = current_version_id_opt {
            // Look up previous current version's hash
            let previous_version_entity = store
                .get(SubgraphVersionEntity::key(current_version_id))?
                .ok_or_else(|| {
                    TransactionAbortError::Other(format!("Subgraph version entity missing"))
                })
                .map_err(StoreError::from)?;
            let previous_version_hash = previous_version_entity
                .get("deployment")
                .unwrap()
                .to_owned()
                .as_string()
                .unwrap();

            // If old current and new current versions have same hash, no need to remove assignment
            if previous_version_hash != manifest.id.to_string() {
                // Find all subgraph versions that point to this hash
                let referencing_version_entities =
                    store.find(SubgraphVersionEntity::query().filter(EntityFilter::Equal(
                        "deployment".to_owned(),
                        previous_version_hash.clone().into(),
                    )))?;
                let referencing_version_entity_ids = referencing_version_entities
                    .iter()
                    .map(|entity| entity.id().unwrap())
                    .collect::<Vec<_>>();

                // Find all subgraphs that have one of these versions as currentVersion
                let subgraphs_with_current_version_referencing_hash = store.find(
                    SubgraphEntity::query().filter(EntityFilter::In(
                        "currentVersion".to_owned(),
                        referencing_version_entity_ids
                            .into_iter()
                            .map(Value::from)
                            .collect(),
                    )),
                )?;
                let subgraph_ids_with_current_version_referencing_hash =
                    subgraphs_with_current_version_referencing_hash
                        .iter()
                        .map(|entity| entity.id().unwrap())
                        .collect::<Vec<_>>();

                // If this subgraph is the only one with this hash as the current version
                if subgraph_ids_with_current_version_referencing_hash.len() == 1 {
                    ops.push(EntityOperation::Remove {
                        key: SubgraphDeploymentAssignmentEntity::key(
                            SubgraphDeploymentId::new(previous_version_hash).unwrap(),
                        ),
                    });
                }
            }
        }
    }

    // Check if assignment already exists for this hash
    let assignment_entity_opt =
        store.get(SubgraphDeploymentAssignmentEntity::key(manifest.id.clone()))?;
    ops.push(EntityOperation::AbortUnless {
        description: "Subgraph assignment entity must continue to exist/not exist".to_owned(),
        query: SubgraphDeploymentAssignmentEntity::query().filter(EntityFilter::Equal(
            "id".to_owned(),
            manifest.id.to_string().into(),
        )),
        entity_ids: if assignment_entity_opt.is_some() {
            vec![manifest.id.to_string()]
        } else {
            vec![]
        },
    });

    // Create assignment entity only if it does not exist already
    if assignment_entity_opt.is_none() {
        ops.extend(SubgraphDeploymentAssignmentEntity::new(node_id).write_operations(&manifest.id));
    }

    // TODO support delayed update of currentVersion
    ops.extend(SubgraphEntity::update_current_version_operations(
        &subgraph_entity_id,
        &version_entity_id,
    ));

    // Commit entity ops
    store.apply_entity_operations(ops, EventSource::None)?;

    debug!(
        logger,
        "Wrote new subgraph version to store";
        "subgraph_name" => name.to_string(),
        "subgraph_hash" => manifest.id.to_string()
    );

    Ok(())
}

fn remove_subgraph(
    logger: &Logger,
    store: Arc<impl Store>,
    name: SubgraphName,
) -> Result<(), SubgraphRegistrarError> {
    let mut ops = vec![];

    // Find the subgraph entity
    let subgraph_entity_opt = store
        .find_one(SubgraphEntity::query().filter(EntityFilter::Equal(
            "name".to_owned(),
            name.to_string().into(),
        )))
        .map_err(|e| format_err!("query execution error: {}", e))?;
    let subgraph_entity = subgraph_entity_opt
        .ok_or_else(|| SubgraphRegistrarError::NameNotFound(name.to_string()))?;

    ops.push(EntityOperation::AbortUnless {
        description: "Subgraph entity must still exist".to_owned(),
        query: SubgraphEntity::query().filter(EntityFilter::Equal(
            "name".to_owned(),
            name.to_string().into(),
        )),
        entity_ids: vec![subgraph_entity.id().unwrap()],
    });

    // Find subgraph version entities
    let subgraph_version_entities = store.find(SubgraphVersionEntity::query().filter(
        EntityFilter::Equal("subgraph".to_owned(), subgraph_entity.id().unwrap().into()),
    ))?;

    ops.push(EntityOperation::AbortUnless {
        description: "Subgraph must have same set of versions".to_owned(),
        query: SubgraphVersionEntity::query().filter(EntityFilter::Equal(
            "subgraph".to_owned(),
            subgraph_entity.id().unwrap().into(),
        )),
        entity_ids: subgraph_version_entities
            .iter()
            .map(|entity| entity.id().unwrap())
            .collect(),
    });

    // Remove subgraph version entities, and their deployment/assignment when applicable
    ops.extend(remove_subgraph_versions(
        store.clone(),
        subgraph_version_entities,
    )?);

    // Remove the subgraph entity
    ops.push(EntityOperation::Remove {
        key: SubgraphEntity::key(subgraph_entity.id()?),
    });

    store.apply_entity_operations(ops, EventSource::None)?;

    debug!(logger, "Removed subgraph"; "subgraph_name" => name.to_string());

    Ok(())
}

/// Remove a set of subgraph versions atomically.
///
/// It may seem like it would be easier to generate the EntityOperations for subgraph versions
/// removal one at a time, but that approach is significantly complicated by the fact that the
/// store does not reflect the EntityOperations that have been accumulated so far. Earlier subgraph
/// version creations/removals can affect later ones by affecting whether or not a subgraph deployment
/// or assignment needs to be created/removed.
fn remove_subgraph_versions(
    store: Arc<impl Store>,
    version_entities_to_delete: Vec<Entity>,
) -> Result<Vec<EntityOperation>, SubgraphRegistrarError> {
    let mut ops = vec![];

    let version_entity_ids_to_delete = version_entities_to_delete
        .iter()
        .map(|version_entity| version_entity.id().unwrap())
        .collect::<HashSet<_>>();

    // Get hashes that are referenced by versions that will be deleted.
    // These are candidates for clean up.
    let referenced_subgraph_hashes = version_entities_to_delete
        .iter()
        .map(|version_entity| {
            version_entity
                .get("deployment")
                .unwrap()
                .to_owned()
                .as_string()
                .unwrap()
        })
        .collect::<HashSet<_>>();

    // Figure out which subgraph deployments need to be removed.
    // A subgraph deployment entity should be removed when the subgraph hash will no longer be
    // referenced by any subgraph versions.
    let all_referencing_versions =
        store.find(SubgraphVersionEntity::query().filter(EntityFilter::In(
            "deployment".to_owned(),
            referenced_subgraph_hashes.iter().map(Value::from).collect(),
        )))?;
    let all_referencing_version_ids = all_referencing_versions
        .iter()
        .map(|version_entity| version_entity.id().unwrap())
        .collect::<Vec<_>>();
    ops.push(EntityOperation::AbortUnless {
        description: "Same set of subgraph versions must reference these subgraph hashes"
            .to_owned(),
        query: SubgraphVersionEntity::query().filter(EntityFilter::In(
            "deployment".to_owned(),
            referenced_subgraph_hashes.iter().map(Value::from).collect(),
        )),
        entity_ids: all_referencing_version_ids.clone(),
    });
    let all_referencing_versions_after_delete = all_referencing_versions
        .clone()
        .into_iter()
        .filter(|version_entity| {
            !version_entity_ids_to_delete.contains(&version_entity.id().unwrap())
        })
        .collect::<Vec<_>>();
    let subgraph_deployment_hashes_needing_deletion = &referenced_subgraph_hashes
        - &all_referencing_versions_after_delete
            .iter()
            .map(|version_entity| {
                version_entity
                    .get("deployment")
                    .unwrap()
                    .to_owned()
                    .as_string()
                    .unwrap()
            })
            .collect::<HashSet<_>>();

    // Remove subgraph deployment entities based on subgraph_deployment_hashes_needing_deletion
    ops.extend(
        subgraph_deployment_hashes_needing_deletion
            .into_iter()
            .map(|subgraph_hash| EntityOperation::Remove {
                key: SubgraphDeploymentEntity::key(
                    SubgraphDeploymentId::new(subgraph_hash).unwrap(),
                ),
            }),
    );

    // Figure out which subgraph assignments need to be removed.
    // A subgraph assignment entity should be removed when the subgraph hash will no longer be
    // referenced by any "current" subgraph version.
    // A "current" subgraph version is a subgraph version that is pointed to by a subgraph's
    // `currentVersion` field.
    let subgraphs_with_referencing_version_as_current = store.find(
        SubgraphEntity::query().filter(EntityFilter::In(
            "currentVersion".to_owned(),
            all_referencing_version_ids
                .iter()
                .map(Value::from)
                .collect(),
        )),
    )?;
    let subgraph_ids_with_referencing_version_as_current =
        subgraphs_with_referencing_version_as_current
            .iter()
            .map(|subgraph_entity| subgraph_entity.id().unwrap())
            .collect::<HashSet<_>>();
    ops.push(EntityOperation::AbortUnless {
        description: "Same set of subgraphs must have these versions as current".to_owned(),
        query: SubgraphEntity::query().filter(EntityFilter::In(
            "currentVersion".to_owned(),
            all_referencing_version_ids
                .iter()
                .map(Value::from)
                .collect(),
        )),
        entity_ids: subgraph_ids_with_referencing_version_as_current
            .into_iter()
            .collect(),
    });
    let all_current_referencing_versions = all_referencing_versions
        .into_iter()
        .filter(|version_entity| {
            subgraphs_with_referencing_version_as_current
                .iter()
                .any(|subgraph_entity| {
                    subgraph_entity.get("currentVersion").unwrap()
                        == version_entity.get("id").unwrap()
                })
        })
        .collect::<Vec<_>>();
    let all_current_referencing_versions_after_delete = all_current_referencing_versions
        .clone()
        .into_iter()
        .filter(|version_entity| {
            !version_entity_ids_to_delete.contains(&version_entity.id().unwrap())
        })
        .collect::<Vec<_>>();
    let subgraph_assignment_hashes_needing_deletion = &referenced_subgraph_hashes
        - &all_current_referencing_versions_after_delete
            .iter()
            .map(|version_entity| {
                version_entity
                    .get("deployment")
                    .unwrap()
                    .to_owned()
                    .as_string()
                    .unwrap()
            })
            .collect::<HashSet<_>>();

    // Remove subgraph assignment entities based on subgraph_assignment_hashes_needing_deletion
    ops.extend(
        subgraph_assignment_hashes_needing_deletion
            .into_iter()
            .map(|subgraph_hash| EntityOperation::Remove {
                key: SubgraphDeploymentAssignmentEntity::key(
                    SubgraphDeploymentId::new(subgraph_hash).unwrap(),
                ),
            }),
    );

    // Remove subgraph version entities, now that they no longer need to be queried
    ops.extend(
        version_entities_to_delete
            .iter()
            .map(|version_entity| EntityOperation::Remove {
                key: SubgraphVersionEntity::key(version_entity.id().unwrap()),
            }),
    );

    Ok(ops)
}
