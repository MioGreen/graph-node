use diesel::debug_query;
use diesel::dsl::sql;
use diesel::pg::Pg;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::r2d2::{self, ConnectionManager, Pool};
use diesel::sql_types::Text;
use diesel::{delete, insert_into, select, update};
use filter::store_filter;
use futures::sync::mpsc::{channel, Sender};
use lru_time_cache::LruCache;
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

use graph::components::store::Store as StoreTrait;
use graph::data::subgraph::schema::*;
use graph::prelude::*;
use graph::serde_json;
use graph::web3::types::H256;
use graph::{tokio, tokio::timer::Interval};
use graph_graphql::prelude::api_schema;

use chain_head_listener::ChainHeadUpdateListener;
use entity_changes::EntityChangeListener;
use functions::{attempt_chain_head_update, lookup_ancestor_block, revert_block, set_config};

embed_migrations!("./migrations");

/// Internal representation of a Store subscription.
struct Subscription {
    pub entities: Vec<SubgraphEntityPair>,
    pub sender: Sender<EntityChange>,
}

/// Run all initial schema migrations.
///
/// Creates the "entities" table if it doesn't already exist.
fn initiate_schema(logger: &Logger, conn: &PgConnection) {
    // Collect migration logging output
    let mut output = vec![];

    match embedded_migrations::run_with_output(conn, &mut output) {
        Ok(_) => info!(logger, "Completed pending Postgres schema migrations"),
        Err(e) => panic!(
            "Error setting up Postgres database: \
             You may need to drop and recreate your database to work with the \
             latest version of graph-node. Error information: {:?}",
            e
        ),
    }

    // If there was any migration output, log it now
    if !output.is_empty() {
        debug!(logger, "Postgres migration output";
               "output" => String::from_utf8(output).unwrap_or_else(|_| String::from("<unreadable>")));
    }
}

/// Configuration for the Diesel/Postgres store.
pub struct StoreConfig {
    pub postgres_url: String,
    pub network_name: String,
}

/// A Store based on Diesel and Postgres.
pub struct Store {
    logger: Logger,
    subscriptions: Arc<RwLock<HashMap<String, Subscription>>>,
    change_listener: EntityChangeListener,
    postgres_url: String,
    network_name: String,
    genesis_block_ptr: EthereumBlockPointer,
    conn: Pool<ConnectionManager<PgConnection>>,
    schema_cache: Mutex<LruCache<SubgraphDeploymentId, Schema>>,
}

impl Store {
    pub fn new(
        config: StoreConfig,
        logger: &Logger,
        net_identifiers: EthereumNetworkIdentifier,
    ) -> Self {
        // Create a store-specific logger
        let logger = logger.new(o!("component" => "Store"));

        #[derive(Debug)]
        struct ErrorHandler(Logger);
        impl r2d2::HandleError<r2d2::Error> for ErrorHandler {
            fn handle_error(&self, error: r2d2::Error) {
                error!(self.0, "Postgres connection error"; "error" => error.to_string())
            }
        }
        let error_handler = Box::new(ErrorHandler(logger.clone()));

        // Connect to Postgres
        let conn_manager = ConnectionManager::new(config.postgres_url.as_str());
        let pool = Pool::builder()
            .error_handler(error_handler)
            .build(conn_manager)
            .unwrap();
        info!(logger, "Connected to Postgres"; "url" => &config.postgres_url);

        // Create the entities table (if necessary)
        initiate_schema(&logger, &pool.get().unwrap());

        // Listen to entity changes in Postgres
        let mut change_listener = EntityChangeListener::new(config.postgres_url.clone());
        let entity_changes = change_listener
            .take_event_stream()
            .expect("Failed to listen to entity change events in Postgres");

        // Create the store
        let mut store = Store {
            logger: logger.clone(),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            change_listener,
            postgres_url: config.postgres_url.clone(),
            network_name: config.network_name.clone(),
            genesis_block_ptr: (net_identifiers.genesis_block_hash, 0u64).into(),
            conn: pool,
            schema_cache: Mutex::new(LruCache::with_capacity(100)),
        };

        // Add network to store and check network identifiers
        store.add_network_if_missing(net_identifiers).unwrap();

        // Deal with store subscriptions
        store.handle_entity_changes(entity_changes);
        store.periodically_clean_up_stale_subscriptions();

        // We're ready for processing entity changes
        store.change_listener.start();

        // Return the store
        store
    }

    fn add_network_if_missing(
        &self,
        new_net_identifiers: EthereumNetworkIdentifier,
    ) -> Result<(), Error> {
        use db_schema::ethereum_networks::dsl::*;

        let new_genesis_block_hash = new_net_identifiers.genesis_block_hash;
        let new_net_version = new_net_identifiers.net_version;

        let network_identifiers_opt = ethereum_networks
            .select((net_version, genesis_block_hash))
            .filter(name.eq(&self.network_name))
            .first::<(Option<String>, Option<String>)>(&*self.conn.get()?)
            .optional()?;

        match network_identifiers_opt {
            // Network is missing in database
            None => {
                insert_into(ethereum_networks)
                    .values((
                        name.eq(&self.network_name),
                        head_block_hash.eq::<Option<String>>(None),
                        head_block_number.eq::<Option<i64>>(None),
                        net_version.eq::<Option<String>>(Some(new_net_version.to_owned())),
                        genesis_block_hash
                            .eq::<Option<String>>(Some(format!("{:x}", new_genesis_block_hash))),
                    ))
                    .on_conflict(name)
                    .do_nothing()
                    .execute(&*self.conn.get()?)?;
            }

            // Network is in database and has identifiers
            Some((Some(last_net_version), Some(last_genesis_block_hash))) => {
                if last_net_version != new_net_version {
                    panic!(
                        "Ethereum node provided net_version {}, \
                         but we expected {}. Did you change networks \
                         without changing the network name?",
                        new_net_version, last_net_version
                    );
                }

                if last_genesis_block_hash.parse().ok() != Some(new_genesis_block_hash) {
                    panic!(
                        "Ethereum node provided genesis block hash {}, \
                         but we expected {}. Did you change networks \
                         without changing the network name?",
                        new_genesis_block_hash, last_genesis_block_hash
                    );
                }
            }

            // Network is in database but is missing identifiers
            Some(_) => {
                update(ethereum_networks)
                    .set((
                        net_version.eq::<Option<String>>(Some(new_net_version.to_owned())),
                        genesis_block_hash
                            .eq::<Option<String>>(Some(format!("{:x}", new_genesis_block_hash))),
                    ))
                    .filter(name.eq(&self.network_name))
                    .execute(&*self.conn.get()?)?;
            }
        }

        Ok(())
    }

    /// Handles entity changes emitted by Postgres.
    fn handle_entity_changes(
        &self,
        entity_changes: Box<Stream<Item = EntityChange, Error = ()> + Send>,
    ) {
        let logger = self.logger.clone();
        let subscriptions = self.subscriptions.clone();

        tokio::spawn(entity_changes.for_each(move |change| {
            trace!(logger, "Received entity change event";
                           "subgraph_id" => change.subgraph_id.to_string(),
                           "entity_type" => &change.entity_type,
                           "entity_id" => &change.entity_id);

            // Obtain IDs and senders of subscriptions matching the entity change
            let matches = subscriptions
                .read()
                .unwrap()
                .iter()
                .filter(|(_, subscription)| {
                    subscription
                        .entities
                        .contains(&(change.subgraph_id.clone(), change.entity_type.clone()))
                })
                .map(|(id, subscription)| (id.clone(), subscription.sender.clone()))
                .collect::<Vec<_>>();

            let subscriptions = subscriptions.clone();
            let logger = logger.clone();

            // Write change to all matching subscription streams; remove subscriptions
            // whose receiving end has been dropped
            stream::iter_ok::<_, ()>(matches).for_each(move |(id, sender)| {
                let logger = logger.clone();
                let subscriptions = subscriptions.clone();
                sender.send(change.clone()).then(move |result| {
                    match result {
                        Err(_send_error) => {
                            // Receiver was dropped
                            debug!(logger, "Unsubscribe"; "id" => &id);
                            subscriptions.write().unwrap().remove(&id);
                            Ok(())
                        }
                        Ok(_sender) => Ok(()),
                    }
                })
            })
        }));
    }

    fn periodically_clean_up_stale_subscriptions(&self) {
        let logger = self.logger.clone();
        let subscriptions = self.subscriptions.clone();

        // Clean up stale subscriptions every 5s
        tokio::spawn(
            Interval::new(Instant::now(), Duration::from_secs(5))
                .for_each(move |_| {
                    let mut subscriptions = subscriptions.write().unwrap();

                    // Obtain IDs of subscriptions whose receiving end has gone
                    let stale_ids = subscriptions
                        .iter_mut()
                        .filter_map(
                            |(id, subscription)| match subscription.sender.poll_ready() {
                                Err(_) => Some(id.clone()),
                                _ => None,
                            },
                        )
                        .collect::<Vec<_>>();

                    // Remove all stale subscriptions
                    for id in stale_ids {
                        debug!(logger, "Unsubscribe"; "id" => &id);
                        subscriptions.remove(&id);
                    }

                    Ok(())
                })
                .map_err(|_| unreachable!()),
        );
    }

    /// Gets an entity from Postgres, returns an entity with just an ID if none is found.
    fn get_entity(
        &self,
        conn: &PgConnection,
        op_subgraph: &SubgraphDeploymentId,
        op_entity: &String,
        op_id: &String,
    ) -> Result<Option<Entity>, QueryExecutionError> {
        use db_schema::entities::dsl::*;

        match entities
            .find((op_id, op_subgraph.to_string(), op_entity))
            .select(data)
            .first::<serde_json::Value>(conn)
            .optional()
            .map_err(|e| {
                QueryExecutionError::ResolveEntityError(
                    op_subgraph.clone(),
                    op_entity.clone(),
                    op_id.clone(),
                    format!("{}", e),
                )
            })? {
            Some(json) => serde_json::from_value::<Entity>(json)
                .map(Some)
                .map_err(|e| {
                    QueryExecutionError::ResolveEntityError(
                        op_subgraph.clone(),
                        op_entity.clone(),
                        op_id.clone(),
                        format!("Invalid entity: {}", e),
                    )
                }),
            None => Ok(None),
        }
    }

    fn execute_query(
        &self,
        conn: &PgConnection,
        query: EntityQuery,
    ) -> Result<Vec<Entity>, QueryExecutionError> {
        use db_schema::entities::dsl::*;

        // Create base boxed query; this will be added to based on the
        // query parameters provided
        let mut diesel_query = entities
            .filter(entity.eq(query.entity_type))
            .filter(subgraph.eq(query.subgraph_id.to_string()))
            .select(data)
            .into_boxed::<Pg>();

        // Add specified filter to query
        if let Some(filter) = query.filter {
            diesel_query = store_filter(diesel_query, filter).map_err(|e| {
                QueryExecutionError::FilterNotSupportedError(format!("{}", e.value), e.filter)
            })?;
        }

        // Add order by filters to query
        if let Some((order_attribute, value_type)) = query.order_by {
            let direction = query
                .order_direction
                .map(|direction| match direction {
                    EntityOrder::Ascending => "ASC",
                    EntityOrder::Descending => "DESC",
                })
                .unwrap_or("ASC");
            let cast_type = match value_type {
                ValueType::BigInt => "::numeric",
                ValueType::Boolean => "::boolean",
                ValueType::Bytes => "",
                ValueType::Float => "::float",
                ValueType::ID => "",
                ValueType::Int => "::bigint",
                ValueType::String => "",
            };
            diesel_query = diesel_query.order(
                sql::<Text>("(data ->")
                    .bind::<Text, _>(order_attribute)
                    .sql("->> 'data')")
                    .sql(cast_type)
                    .sql(" ")
                    .sql(direction)
                    .sql(" NULLS LAST"),
            );
        }

        // Add range filter to query
        if let Some(range) = query.range {
            diesel_query = diesel_query
                .limit(range.first as i64)
                .offset(range.skip as i64);
        }

        // Record debug info in case of error
        let diesel_query_debug_info = debug_query(&diesel_query).to_string();

        // Process results; deserialize JSON data
        diesel_query
            .load::<serde_json::Value>(conn)
            .map(|values| {
                values
                    .into_iter()
                    .map(|value| {
                        let parse_error_msg = format!("Error parsing entity JSON: {:?}", value);
                        serde_json::from_value::<Entity>(value).expect(&parse_error_msg)
                    })
                    .collect()
            })
            .map_err(|e| {
                QueryExecutionError::ResolveEntitiesError(format!(
                    "{}, query = {:?}",
                    e, diesel_query_debug_info
                ))
            })
    }

    /// Applies a set operation in Postgres.
    fn apply_set_operation(
        &self,
        conn: &PgConnection,
        key: EntityKey,
        data: Entity,
        event_source: EventSource,
    ) -> Result<(), StoreError> {
        use db_schema::entities;

        // Load the entity if exists
        let existing_entity = self
            .get_entity(conn, &key.subgraph_id, &key.entity_type, &key.entity_id)
            .map_err(Error::from)?;

        // Apply the operation
        let operation = EntityOperation::Set {
            key: key.clone(),
            data,
        };
        let updated_entity = operation.apply(existing_entity)?;
        let updated_json: serde_json::Value =
            serde_json::to_value(&updated_entity).map_err(|e| {
                format_err!(
                    "Failed to set entity ({}, {}, {}) as setting it would break it: {}",
                    key.subgraph_id,
                    key.entity_type,
                    key.entity_id,
                    e
                )
            })?;

        // Either add or update the entity in Postgres
        insert_into(entities::table)
            .values((
                entities::id.eq(&key.entity_id),
                entities::entity.eq(&key.entity_type),
                entities::subgraph.eq(key.subgraph_id.to_string()),
                entities::data.eq(&updated_json),
                entities::event_source.eq(event_source.to_string()),
            ))
            .on_conflict((entities::id, entities::entity, entities::subgraph))
            .do_update()
            .set((
                entities::id.eq(&key.entity_id),
                entities::entity.eq(&key.entity_type),
                entities::subgraph.eq(key.subgraph_id.to_string()),
                entities::data.eq(&updated_json),
                entities::event_source.eq(event_source.to_string()),
            ))
            .execute(conn)
            .map(|_| ())
            .map_err(|e| {
                format_err!(
                    "Failed to set entity ({}, {}, {}): {}",
                    key.subgraph_id,
                    key.entity_type,
                    key.entity_id,
                    e
                )
                .into()
            })
    }

    /// Applies a remove operation by deleting the entity from Postgres.
    fn apply_remove_operation(
        &self,
        conn: &PgConnection,
        key: EntityKey,
        event_source: EventSource,
    ) -> Result<(), StoreError> {
        use db_schema::entities;

        select(set_config(
            "vars.current_event_source",
            event_source.to_string(),
            true,
        ))
        .execute(conn)
        .map_err(|e| format_err!("Failed to save event source for remove operation: {}", e))
        .map(|_| ())?;

        delete(
            entities::table
                .filter(entities::subgraph.eq(key.subgraph_id.to_string()))
                .filter(entities::entity.eq(&key.entity_type))
                .filter(entities::id.eq(&key.entity_id)),
        )
        .execute(conn)
        .map(|_| ())
        .map_err(|e| {
            format_err!(
                "Failed to remove entity ({}, {}, {}): {}",
                key.subgraph_id,
                key.entity_type,
                key.entity_id,
                e
            )
            .into()
        })
    }

    fn apply_abort_unless_operation(
        &self,
        conn: &PgConnection,
        description: String,
        query: EntityQuery,
        mut expected_entity_ids: Vec<String>,
        _event_source: EventSource,
    ) -> Result<(), StoreError> {
        if query.range.is_some() && query.order_by.is_none() {
            // Queries with a range but no sort key can vary non-deterministically in what they
            // return, and so are not suitable for use with AbortUnless.
            panic!("Cannot use range in an AbortUnless query without order_by");
        }

        // Execute query
        let actual_entities = self.execute_query(conn, query.clone()).map_err(|e| {
            format_err!(
                "AbortUnless ({}): query execution error: {:?}, {}",
                description,
                query,
                e
            )
        })?;

        // Extract IDs from entities
        let mut actual_entity_ids: Vec<String> = actual_entities
            .into_iter()
            .map(|entity| entity.id())
            .collect::<Result<_, _>>()?;

        // Sort entity IDs lexicographically if and only if no sort order is specified.
        // When no sort order is specified, the entity ordering is arbitrary and should not be a
        // factor in deciding whether or not to abort.
        if query.order_by.is_none() {
            expected_entity_ids.sort();
            actual_entity_ids.sort();
        }

        // Abort if actual IDs do not match expected
        if actual_entity_ids != expected_entity_ids {
            return Err(TransactionAbortError::AbortUnless {
                expected_entity_ids,
                actual_entity_ids,
                description,
            }
            .into());
        }

        // Safe to continue
        Ok(())
    }

    /// Apply an entity operation in Postgres.
    fn apply_entity_operation(
        &self,
        conn: &PgConnection,
        operation: EntityOperation,
        event_source: EventSource,
    ) -> Result<(), StoreError> {
        match operation {
            EntityOperation::Set { key, data } => {
                self.apply_set_operation(conn, key, data, event_source)
            }
            EntityOperation::Remove { key } => self.apply_remove_operation(conn, key, event_source),
            EntityOperation::AbortUnless {
                description,
                query,
                entity_ids,
            } => self.apply_abort_unless_operation(
                conn,
                description,
                query,
                entity_ids,
                event_source,
            ),
        }
    }

    /// Apply a series of entity operations in Postgres.
    fn apply_entity_operations_with_conn(
        &self,
        conn: &PgConnection,
        operations: Vec<EntityOperation>,
        event_source: EventSource,
    ) -> Result<(), StoreError> {
        for operation in operations.into_iter() {
            self.apply_entity_operation(conn, operation, event_source)?;
        }
        Ok(())
    }
}

impl StoreTrait for Store {
    fn block_ptr(&self, subgraph_id: SubgraphDeploymentId) -> Result<EthereumBlockPointer, Error> {
        let subgraph_entity = self
            .get(SubgraphDeploymentEntity::key(subgraph_id.clone()))
            .map_err(|e| format_err!("error reading subgraph entity: {}", e))?
            .ok_or_else(|| {
                format_err!(
                    "could not read block ptr for non-existent subgraph {}",
                    subgraph_id
                )
            })?;

        let hash = subgraph_entity
            .get("latestEthereumBlockHash")
            .ok_or_else(|| format_err!("SubgraphDeployment is missing latestEthereumBlockHash"))?
            .to_owned()
            .as_string()
            .ok_or_else(|| {
                format_err!("SubgraphDeployment has wrong type in latestEthereumBlockHash")
            })?
            .parse::<H256>()
            .map_err(|e| format_err!("latestEthereumBlockHash: {}", e))?;

        let number = subgraph_entity
            .get("latestEthereumBlockNumber")
            .ok_or_else(|| format_err!("SubgraphDeployment is missing latestEthereumBlockNumber"))?
            .to_owned()
            .as_bigint()
            .ok_or_else(|| {
                format_err!("SubgraphDeployment has wrong type in latestEthereumBlockNumber")
            })?
            .to_u64();

        Ok(EthereumBlockPointer { hash, number })
    }

    fn get(&self, key: EntityKey) -> Result<Option<Entity>, QueryExecutionError> {
        let conn = self
            .conn
            .get()
            .map_err(|e| QueryExecutionError::StoreError(e.into()))?;
        self.get_entity(&*conn, &key.subgraph_id, &key.entity_type, &key.entity_id)
    }

    fn find(&self, query: EntityQuery) -> Result<Vec<Entity>, QueryExecutionError> {
        let conn = self
            .conn
            .get()
            .map_err(|e| QueryExecutionError::StoreError(e.into()))?;
        self.execute_query(&conn, query)
    }

    fn find_one(&self, mut query: EntityQuery) -> Result<Option<Entity>, QueryExecutionError> {
        if let Some(mut range) = query.range.clone() {
            if range.first == 0 {
                return Ok(None);
            }

            range.first = 1;
            query.range = Some(range);
        } else {
            query.range = Some(EntityRange { first: 1, skip: 0 })
        }

        let conn = self
            .conn
            .get()
            .map_err(|e| QueryExecutionError::StoreError(e.into()))?;

        let mut results = self.execute_query(&conn, query)?;
        match results.len() {
            0 | 1 => Ok(results.pop()),
            n => panic!("find_one query found {} results", n),
        }
    }

    fn set_block_ptr_with_no_changes(
        &self,
        subgraph_id: SubgraphDeploymentId,
        block_ptr_from: EthereumBlockPointer,
        block_ptr_to: EthereumBlockPointer,
    ) -> Result<(), StoreError> {
        let ops = SubgraphDeploymentEntity::update_ethereum_block_pointer_operations(
            &subgraph_id,
            block_ptr_from,
            block_ptr_to,
        );
        self.apply_entity_operations(ops, EventSource::None)
    }

    fn transact_block_operations(
        &self,
        subgraph_id: SubgraphDeploymentId,
        block_ptr_from: EthereumBlockPointer,
        block_ptr_to: EthereumBlockPointer,
        mut operations: Vec<EntityOperation>,
    ) -> Result<(), StoreError> {
        // Sanity check on block numbers
        if block_ptr_from.number != block_ptr_to.number - 1 {
            panic!("transact_block_operations must transact a single block only");
        }

        // All operations should apply only to entities in this subgraph
        for op in &operations {
            if op.entity_key().subgraph_id != subgraph_id {
                panic!("transact_block_operations must affect only entities in the subgraph");
            }
        }

        // Update subgraph block pointer in same transaction
        operations.append(
            &mut SubgraphDeploymentEntity::update_ethereum_block_pointer_operations(
                &subgraph_id,
                block_ptr_from,
                block_ptr_to,
            ),
        );

        let event_source = EventSource::EthereumBlock(block_ptr_to);
        self.apply_entity_operations(operations, event_source)
    }

    fn apply_entity_operations(
        &self,
        operations: Vec<EntityOperation>,
        event_source: EventSource,
    ) -> Result<(), StoreError> {
        let conn = self.conn.get().map_err(Error::from)?;
        conn.transaction(|| self.apply_entity_operations_with_conn(&conn, operations, event_source))
    }

    fn revert_block_operations(
        &self,
        subgraph_id: SubgraphDeploymentId,
        block_ptr_from: EthereumBlockPointer,
        block_ptr_to: EthereumBlockPointer,
    ) -> Result<(), StoreError> {
        // Sanity check on block numbers
        if block_ptr_from.number != block_ptr_to.number + 1 {
            panic!("revert_block_operations must revert a single block only");
        }

        let conn = self.conn.get().map_err(Error::from)?;
        conn.transaction(|| {
            let ops = SubgraphDeploymentEntity::update_ethereum_block_pointer_operations(
                &subgraph_id,
                block_ptr_from,
                block_ptr_to,
            );
            self.apply_entity_operations_with_conn(&conn, ops, EventSource::None)?;

            select(revert_block(
                &block_ptr_from.hash_hex(),
                block_ptr_from.number as i64,
                &block_ptr_to.hash_hex(),
                subgraph_id.to_string(),
            ))
            .execute(&*conn)
            .map(|_| ())
            .map_err(|e| format_err!("Error reverting block: {}", e).into())
        })
    }

    fn subscribe(&self, entities: Vec<SubgraphEntityPair>) -> EntityChangeStream {
        let subscriptions = self.subscriptions.clone();

        // Generate a new (unique) UUID; we're looping just to be sure we avoid collisions
        let mut id = Uuid::new_v4().to_string();
        while subscriptions.read().unwrap().contains_key(&id) {
            id = Uuid::new_v4().to_string();
        }

        debug!(self.logger, "Subscribe";
               "id" => &id,
               "entities" => format!("{:?}", entities));

        // Prepare the new subscription by creating a channel and a subscription object
        let (sender, receiver) = channel(100);
        let subscription = Subscription { entities, sender };

        // Add the new subscription
        let mut subscriptions = subscriptions.write().unwrap();
        subscriptions.insert(id, subscription);

        // Return the subscription ID and entity change stream
        Box::new(receiver)
    }

    fn count_entities(&self, subgraph_id: SubgraphDeploymentId) -> Result<u64, Error> {
        use db_schema::entities::dsl::*;

        let count: i64 = entities
            .filter(subgraph.eq(subgraph_id.to_string()))
            .count()
            .get_result(&*self.conn.get()?)?;
        Ok(count as u64)
    }
}

impl SubgraphDeploymentStore for Store {
    fn resolve_subgraph_name_to_id(
        &self,
        name: SubgraphName,
    ) -> Result<Option<SubgraphDeploymentId>, Error> {
        // Find subgraph entity by name
        let subgraph_entities = self
            .find(SubgraphEntity::query().filter(EntityFilter::Equal(
                "name".to_owned(),
                name.to_string().into(),
            )))
            .map_err(QueryError::from)?;
        let subgraph_entity = match subgraph_entities.len() {
            0 => return Ok(None),
            1 => {
                let mut subgraph_entities = subgraph_entities;
                Ok(subgraph_entities.pop().unwrap())
            }
            _ => Err(format_err!(
                "Multiple subgraphs found with name {:?}",
                name.to_string()
            )),
        }?;

        // Get current active subgraph version ID
        let current_version_id = match subgraph_entity
            .get("currentVersion")
            .ok_or_else(|| format_err!("Subgraph entity without `currentVersion`"))?
        {
            Value::String(s) => s.to_owned(),
            Value::Null => return Ok(None),
            _ => {
                return Err(format_err!(
                    "Subgraph entity has wrong type in `currentVersion`"
                ))
            }
        };

        // Read subgraph version entity
        let version_entity_opt = self
            .get(SubgraphVersionEntity::key(current_version_id))
            .map_err(QueryError::from)?;
        if version_entity_opt == None {
            return Ok(None);
        }
        let version_entity = version_entity_opt.unwrap();

        // Parse subgraph ID
        let subgraph_id_str = version_entity
            .get("deployment")
            .ok_or_else(|| format_err!("SubgraphVersion entity without `deployment`"))?
            .to_owned()
            .as_string()
            .ok_or_else(|| format_err!("SubgraphVersion entity has wrong type in `deployment`"))?;
        SubgraphDeploymentId::new(subgraph_id_str)
            .map_err(|()| {
                format_err!("SubgraphVersion entity has invalid subgraph ID in `deployment`")
            })
            .map(Some)
    }

    fn is_deployed(&self, id: &SubgraphDeploymentId) -> Result<bool, Error> {
        // The subgraph of subgraphs is always deployed.
        if id == &*SUBGRAPHS_ID {
            return Ok(true);
        }

        // Check store for a deployment entity for this subgraph ID
        self.get(SubgraphDeploymentEntity::key(id.to_owned()))
            .map_err(|e| format_err!("Failed to query SubgraphDeployment entities: {}", e))
            .map(|entity_opt| entity_opt.is_some())
    }

    fn subgraph_schema(&self, subgraph_id: SubgraphDeploymentId) -> Result<Schema, Error> {
        if let Some(schema) = self.schema_cache.lock().unwrap().get(&subgraph_id) {
            trace!(self.logger, "schema cache hit"; "id" => subgraph_id.to_string());
            return Ok(schema.clone());
        }
        trace!(self.logger, "schema cache miss"; "id" => subgraph_id.to_string());

        let raw_schema = if subgraph_id == *SUBGRAPHS_ID {
            // The subgraph of subgraphs schema is built-in.
            include_str!("subgraphs.graphql").to_owned()
        } else {
            let manifest_entity = self
                .get(EntityKey {
                    subgraph_id: SUBGRAPHS_ID.clone(),
                    entity_type: SubgraphManifestEntity::TYPENAME.to_owned(),
                    entity_id: SubgraphManifestEntity::id(&subgraph_id),
                })?
                .ok_or_else(|| format_err!("Subgraph entity not found {}", subgraph_id))?;

            match manifest_entity.get("schema") {
                Some(Value::String(raw)) => raw.clone(),
                _ => {
                    return Err(format_err!(
                        "Schema not present or has wrong type, subgraph: {}",
                        subgraph_id
                    ))
                }
            }
        };
        let mut schema = Schema::parse(&raw_schema, subgraph_id.clone())?;
        schema.document = api_schema(&schema.document)?;

        if !self.schema_cache.lock().unwrap().contains_key(&subgraph_id) {
            self.schema_cache
                .lock()
                .unwrap()
                .insert(subgraph_id, schema.clone());
        }

        Ok(schema)
    }
}

impl ChainStore for Store {
    type ChainHeadUpdateListener = ChainHeadUpdateListener;

    fn genesis_block_ptr(&self) -> Result<EthereumBlockPointer, Error> {
        Ok(self.genesis_block_ptr)
    }

    fn upsert_blocks<'a, B, E>(&self, blocks: B) -> Box<Future<Item = (), Error = E> + Send + 'a>
    where
        B: Stream<Item = EthereumBlock, Error = E> + Send + 'a,
        E: From<Error> + Send + 'a,
    {
        use db_schema::ethereum_blocks::dsl::*;

        let conn = self.conn.clone();
        let net_name = self.network_name.clone();
        Box::new(blocks.for_each(move |block| {
            let json_blob = serde_json::to_value(&block).expect("Failed to serialize block");
            let values = (
                hash.eq(format!("{:x}", block.block.hash.unwrap())),
                number.eq(block.block.number.unwrap().as_u64() as i64),
                parent_hash.eq(format!("{:x}", block.block.parent_hash)),
                network_name.eq(&net_name),
                data.eq(json_blob),
            );

            // Insert blocks.
            // If the table already contains a block with the same hash,
            // then overwrite that block (on conflict do update).
            // That case is a no-op because blocks are immutable
            // (unless the Ethereum node returned corrupt data).
            insert_into(ethereum_blocks)
                .values(values.clone())
                .on_conflict(hash)
                .do_update()
                .set(values)
                .execute(&*conn.get().map_err(Error::from)?)
                .map_err(Error::from)
                .map_err(E::from)
                .map(|_| ())
        }))
    }

    fn attempt_chain_head_update(&self, ancestor_count: u64) -> Result<Vec<H256>, Error> {
        // Call attempt_head_update SQL function
        select(attempt_chain_head_update(
            &self.network_name,
            ancestor_count as i64,
        ))
        .load(&*self.conn.get()?)
        .map_err(Error::from)
        // We got a single return value, but it's returned generically as a set of rows
        .map(|mut rows: Vec<_>| {
            assert_eq!(rows.len(), 1);
            rows.pop().unwrap()
        })
        // Parse block hashes into H256 type
        .map(|hashes: Vec<String>| {
            hashes
                .into_iter()
                .map(|h| h.parse())
                .collect::<Result<Vec<H256>, _>>()
        })
        .and_then(|r| r.map_err(Error::from))
    }

    fn chain_head_updates(&self) -> Self::ChainHeadUpdateListener {
        Self::ChainHeadUpdateListener::new(self.postgres_url.clone(), self.network_name.clone())
    }

    fn chain_head_ptr(&self) -> Result<Option<EthereumBlockPointer>, Error> {
        use db_schema::ethereum_networks::dsl::*;

        ethereum_networks
            .select((head_block_hash, head_block_number))
            .filter(name.eq(&self.network_name))
            .load::<(Option<String>, Option<i64>)>(&*self.conn.get()?)
            .map(|rows| {
                rows.first()
                    .map(|(hash_opt, number_opt)| match (hash_opt, number_opt) {
                        (Some(hash), Some(number)) => Some((hash.parse().unwrap(), *number).into()),
                        (None, None) => None,
                        _ => unreachable!(),
                    })
                    .and_then(|opt| opt)
            })
            .map_err(Error::from)
    }

    fn block(&self, block_hash: H256) -> Result<Option<EthereumBlock>, Error> {
        use db_schema::ethereum_blocks::dsl::*;

        ethereum_blocks
            .select(data)
            .filter(network_name.eq(&self.network_name))
            .filter(hash.eq(format!("{:x}", block_hash)))
            .load::<serde_json::Value>(&*self.conn.get()?)
            .map(|json_blocks| match json_blocks.len() {
                0 => None,
                1 => Some(
                    serde_json::from_value::<EthereumBlock>(json_blocks[0].clone())
                        .expect("Failed to deserialize block"),
                ),
                _ => unreachable!(),
            })
            .map_err(Error::from)
    }

    fn ancestor_block(
        &self,
        block_ptr: EthereumBlockPointer,
        offset: u64,
    ) -> Result<Option<EthereumBlock>, Error> {
        if block_ptr.number < offset {
            bail!("block offset points to before genesis block");
        }

        select(lookup_ancestor_block(block_ptr.hash_hex(), offset as i64))
            .first::<Option<serde_json::Value>>(&*self.conn.get()?)
            .map(|val_opt| {
                val_opt.map(|val| {
                    serde_json::from_value::<EthereumBlock>(val)
                        .expect("Failed to deserialize block from database")
                })
            })
            .map_err(Error::from)
    }
}
