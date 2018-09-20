use failure::{Error as FailureError, *};
use futures::sync::mpsc::Sender;
use nan_preserving_float::F64;
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;

use wasmi::{
    Error, Externals, FuncInstance, FuncRef, HostError, ImportsBuilder, MemoryRef, Module,
    ModuleImportResolver, ModuleInstance, ModuleRef, NopExternals, RuntimeArgs, RuntimeValue,
    Signature, Trap, TrapKind, ValueType,
};

use futures::sync::oneshot;
use graph::components::ethereum::*;
use graph::components::store::{EventSource, StoreKey};
use graph::data::store::scalar;
use graph::data::subgraph::DataSource;
use graph::prelude::*;
use graph::serde_json;
use graph::web3::types::{BlockId, H160, H256, U256};

use super::UnresolvedContractCall;

use asc_abi::asc_ptr::*;
use asc_abi::class::*;
use asc_abi::*;
use hex;

/// AssemblyScript-compatible WASM memory heap.
#[derive(Clone)]
struct WasmiAscHeap {
    module: ModuleRef,
    memory: MemoryRef,
}

impl WasmiAscHeap {
    pub fn new(module: ModuleRef, memory: MemoryRef) -> Self {
        WasmiAscHeap { module, memory }
    }
}

impl AscHeap for WasmiAscHeap {
    fn raw_new(&self, bytes: &[u8]) -> Result<u32, Error> {
        let address = self
            .module
            .invoke_export(
                "allocate_memory",
                &[RuntimeValue::I32(bytes.len() as i32)],
                &mut NopExternals,
            ).expect("Failed to invoke memory allocation function")
            .expect("Function did not return a value")
            .try_into::<u32>()
            .expect("Function did not return u32");

        self.memory.set(address, bytes)?;

        Ok(address)
    }

    fn get(&self, offset: u32, size: u32) -> Result<Vec<u8>, Error> {
        self.memory.get(offset, size as usize)
    }
}

// Indexes for exported host functions
const ABORT_FUNC_INDEX: usize = 0;
const STORE_SET_FUNC_INDEX: usize = 1;
const STORE_REMOVE_FUNC_INDEX: usize = 2;
const ETHEREUM_CALL_FUNC_INDEX: usize = 3;
const TYPE_CONVERSION_BYTES_TO_STRING_FUNC_INDEX: usize = 4;
const TYPE_CONVERSION_BYTES_TO_HEX_FUNC_INDEX: usize = 5;
const TYPE_CONVERSION_U64_ARRAY_TO_STRING_FUNC_INDEX: usize = 6;
const TYPE_CONVERSION_U64_ARRAY_TO_HEX_FUNC_INDEX: usize = 7;
const TYPE_CONVERSION_H256_TO_H160_FUNC_INDEX: usize = 8;
const TYPE_CONVERSION_H160_TO_H256_FUNC_INDEX: usize = 9;
const TYPE_CONVERSION_U256_TO_H160_FUNC_INDEX: usize = 10;
const TYPE_CONVERSION_U256_TO_H256_FUNC_INDEX: usize = 11;
const TYPE_CONVERSION_STRING_TO_H160_FUNC_INDEX: usize = 12;
const TYPE_CONVERSION_INT256_TO_BIG_INT_FUNC_INDEX: usize = 13;
const JSON_FROM_BYTES_FUNC_INDEX: usize = 14;
const JSON_TO_I64_FUNC_INDEX: usize = 15;
const JSON_TO_U64_FUNC_INDEX: usize = 16;
const JSON_TO_F64_FUNC_INDEX: usize = 17;
const JSON_TO_BIG_INT_FUNC_INDEX: usize = 18;
const IPFS_CAT_FUNC_INDEX: usize = 19;
const STORE_GET_FUNC_INDEX: usize = 20;
const TYPE_CONVERSION_BIG_INT_FUNC_TO_INT256_INDEX: usize = 21;

pub struct WasmiModuleConfig<T, L, S> {
    pub subgraph: SubgraphManifest,
    pub data_source: DataSource,
    pub ethereum_adapter: Arc<Mutex<T>>,
    pub link_resolver: Arc<L>,
    pub store: Arc<S>,
}

impl<T, L, S> Clone for WasmiModuleConfig<T, L, S> {
    fn clone(&self) -> Self {
        WasmiModuleConfig {
            subgraph: self.subgraph.clone(),
            data_source: self.data_source.clone(),
            ethereum_adapter: self.ethereum_adapter.clone(),
            link_resolver: self.link_resolver.clone(),
            store: self.store.clone(),
        }
    }
}

/// A WASM module based on wasmi that powers a subgraph runtime.
pub struct WasmiModule<T, L, S, U> {
    pub logger: Logger,
    pub module: ModuleRef,
    externals: HostExternals<T, L, S, U>,
    heap: WasmiAscHeap,
}

impl<T, L, S, U> WasmiModule<T, L, S, U>
where
    T: EthereumAdapter,
    L: LinkResolver,
    S: Store + Send + Sync,
    U: Sink<SinkItem = Box<Future<Item = (), Error = ()> + Send>> + Clone,
{
    /// Creates a new wasmi module
    pub fn new(logger: &Logger, config: WasmiModuleConfig<T, L, S>, task_sink: U) -> Self {
        let logger = logger.new(o!("component" => "WasmiModule"));

        let module = Module::from_parity_wasm_module(config.data_source.mapping.runtime.clone())
            .expect(
                format!(
                    "Wasmi could not interpret module of data source: {}",
                    config.data_source.name
                ).as_str(),
            );

        // Build import resolver
        let mut imports = ImportsBuilder::new();
        imports.push_resolver("env", &EnvModuleResolver);
        imports.push_resolver("store", &StoreModuleResolver);
        imports.push_resolver("ethereum", &EthereumModuleResolver);
        imports.push_resolver("typeConversion", &TypeConversionModuleResolver);
        imports.push_resolver("json", &JsonModuleResolver);
        imports.push_resolver("ipfs", &IpfsModuleResolver);

        // Instantiate the runtime module using hosted functions and import resolver
        let module =
            ModuleInstance::new(&module, &imports).expect("Failed to instantiate WASM module");

        // Provide access to the WASM runtime linear memory
        let not_started_module = module.not_started_instance().clone();
        let memory = not_started_module
            .export_by_name("memory")
            .expect("Failed to find memory export in the WASM module")
            .as_memory()
            .expect("Export \"memory\" has an invalid type")
            .clone();

        // Create a AssemblyScript-compatible WASM memory heap
        let heap = WasmiAscHeap::new(not_started_module, memory);

        // Create new instance of externally hosted functions invoker
        let mut externals = HostExternals {
            subgraph: config.subgraph,
            data_source: config.data_source,
            logger: logger.clone(),
            heap: heap.clone(),
            ethereum_adapter: config.ethereum_adapter.clone(),
            link_resolver: config.link_resolver.clone(),
            block_hash: H256::zero(),
            store: config.store.clone(),
            entity_operations: vec![],
            task_sink,
        };

        let module = module
            .run_start(&mut externals)
            .expect("Failed to start WASM module instance");

        WasmiModule {
            logger,
            module,
            externals,
            heap,
        }
    }

    pub fn handle_ethereum_event(
        &mut self,
        handler_name: &str,
        event: EthereumEvent,
    ) -> Result<Vec<EntityOperation>, FailureError> {
        self.externals.block_hash = event.block_hash.clone();

        // Create new vector for entity operations generated while handling this event
        self.externals.entity_operations.clear();

        // Invoke the event handler
        let result = self.module.invoke_export(
            handler_name,
            &[RuntimeValue::from(self.heap.asc_new(&event))],
            &mut self.externals,
        );

        // Return either the collected entity operations or an error
        result
            .map(|_| self.externals.entity_operations.clone())
            .map_err(|e| {
                format_err!(
                    "Failed to handle Ethereum event with handler \"{}\": {}",
                    handler_name,
                    e
                )
            })
    }
}

/// Error raised in host functions.
#[derive(Debug)]
struct HostExternalsError<E>(E);

impl<E> HostError for HostExternalsError<E> where
    E: fmt::Debug + fmt::Display + Send + Sync + 'static
{}

impl<E: fmt::Display> fmt::Display for HostExternalsError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn host_error(message: String) -> Trap {
    Trap::new(TrapKind::Host(Box::new(HostExternalsError(message))))
}

/// Hosted functions for external use by wasm module
pub struct HostExternals<T, L, S, U> {
    logger: Logger,
    subgraph: SubgraphManifest,
    data_source: DataSource,
    heap: WasmiAscHeap,
    ethereum_adapter: Arc<Mutex<T>>,
    link_resolver: Arc<L>,
    // Block hash of the event being mapped.
    block_hash: H256,
    store: Arc<S>,
    // Entity operations collected while handling events
    entity_operations: Vec<EntityOperation>,
    task_sink: U,
}

impl<T, L, S, U> HostExternals<T, L, S, U>
where
    T: EthereumAdapter,
    L: LinkResolver,
    S: Store + Send + Sync,
    U: Sink<SinkItem = Box<Future<Item = (), Error = ()> + Send>> + Clone,
{
    /// function store.set(entity: string, id: string, data: Entity): void
    fn store_set(
        &mut self,
        entity_ptr: AscPtr<AscString>,
        id_ptr: AscPtr<AscString>,
        data_ptr: AscPtr<AscEntity>,
    ) -> Result<Option<RuntimeValue>, Trap> {
        let block_hash: H256 = self.block_hash.clone();
        let entity: String = self.heap.asc_get(entity_ptr);
        let id: String = self.heap.asc_get(id_ptr);
        let data: HashMap<String, Value> = self.heap.asc_get(data_ptr);

        self.entity_operations.push(EntityOperation::Set {
            subgraph: self.subgraph.id.clone(),
            entity,
            id,
            data: Entity::from(data),
        });

        Ok(None)
    }

    /// function store.remove(entity: string, id: string): void
    fn store_remove(
        &mut self,
        entity_ptr: AscPtr<AscString>,
        id_ptr: AscPtr<AscString>,
    ) -> Result<Option<RuntimeValue>, Trap> {
        let block_hash: H256 = self.block_hash.clone();
        let entity: String = self.heap.asc_get(entity_ptr);
        let id: String = self.heap.asc_get(id_ptr);

        self.entity_operations.push(EntityOperation::Remove {
            subgraph: self.subgraph.id.clone(),
            entity,
            id,
        });

        Ok(None)
    }

    /// function store.get(entity: string, id: string): void
    fn store_get(
        &self,
        entity_ptr: AscPtr<AscString>,
        id_ptr: AscPtr<AscString>,
    ) -> Result<Option<RuntimeValue>, Trap> {
        let store_key = StoreKey {
            subgraph: self.subgraph.id.clone(),
            entity: self.heap.asc_get(entity_ptr),
            id: self.heap.asc_get(id_ptr),
        };

        // Retrieve an Entity from the store
        self.store
            .get(store_key)
            .map_err(|_| host_error("Error getting entity".to_string()))
            .and_then(|result| Ok(Some(RuntimeValue::from(self.heap.asc_new(&result)))))
    }

    /// function ethereum.call(call: SmartContractCall): Array<Token>
    fn ethereum_call(
        &self,
        call_ptr: AscPtr<AscUnresolvedContractCall>,
    ) -> Result<Option<RuntimeValue>, Trap> {
        let unresolved_call: UnresolvedContractCall = self.heap.asc_get(call_ptr);
        info!(self.logger, "Call smart contract";
              "address" => &unresolved_call.contract_address.to_string(),
              "contract" => &unresolved_call.contract_name,
              "function" => &unresolved_call.function_name);

        // Obtain the path to the contract ABI
        let contract = self
            .data_source
            .mapping
            .abis
            .iter()
            .find(|abi| abi.name == unresolved_call.contract_name)
            .ok_or(host_error(format!(
                "Unknown contract \"{}\" called from WASM runtime",
                unresolved_call.contract_name
            )))?.contract
            .clone();

        let function = contract
            .function(unresolved_call.function_name.as_str())
            .map_err(|e| {
                host_error(format!(
                    "Unknown function \"{}::{}\" called from WASM runtime: {}",
                    unresolved_call.contract_name, unresolved_call.function_name, e
                ))
            })?;

        let call = EthereumContractCall {
            address: unresolved_call.contract_address.clone(),
            block_id: BlockId::Hash(self.block_hash.clone()),
            function: function.clone(),
            args: unresolved_call.function_args.clone(),
        };

        self.ethereum_adapter
            .lock()
            .unwrap()
            .contract_call(call)
            .wait()
            .map(|result| Some(RuntimeValue::from(self.heap.asc_new(&*result))))
            .map_err(|e| {
                host_error(format!(
                    "Failed to call function \"{}\" of contract \"{}\": {}",
                    unresolved_call.function_name, unresolved_call.contract_name, e
                ))
            })
    }

    /// function typeConversion.bytesToString(bytes: Bytes): string
    fn convert_bytes_to_string(
        &self,
        bytes_ptr: AscPtr<Uint8Array>,
    ) -> Result<Option<RuntimeValue>, Trap> {
        let bytes: Vec<u8> = self.heap.asc_get(bytes_ptr);
        let s = String::from_utf8_lossy(&*bytes);
        // The string may have been encoded in a fixed length
        // buffer and padded with null characters, so trim
        // trailing nulls.
        let trimmed_s = s.trim_right_matches('\u{0000}');
        Ok(Some(RuntimeValue::from(self.heap.asc_new(trimmed_s))))
    }

    /// function typeConversion.u64ArrayToString(u64_array: U64Array): string
    fn u64_array_to_string(
        &self,
        u64_array_ptr: AscPtr<Uint64Array>,
    ) -> Result<Option<RuntimeValue>, Trap> {
        let u64_array: Vec<u64> = self.heap.asc_get(u64_array_ptr);
        let mut bytes: Vec<u8> = Vec::new();
        for x in u64_array {
            // This is just `x.to_bytes()` which is unstable.
            let x_bytes: [u8; 8] = unsafe { ::std::mem::transmute(x) };
            bytes.extend(x_bytes.iter());
        }

        let s = String::from_utf8_lossy(&*bytes);
        let trimmed_s = s.trim_right_matches('\u{0000}');
        Ok(Some(RuntimeValue::from(self.heap.asc_new(trimmed_s))))
    }

    /// function typeConversion.u64ArrayToHex(u64_array: U64Array): string
    fn u64_array_to_hex(
        &self,
        u64_array_ptr: AscPtr<Uint64Array>,
    ) -> Result<Option<RuntimeValue>, Trap> {
        let u64_array: Vec<u64> = self.heap.asc_get(u64_array_ptr);
        let mut bytes: Vec<u8> = Vec::new();
        for x in u64_array {
            // This is just `x.to_bytes()` which is unstable.
            let x_bytes: [u8; 8] = unsafe { ::std::mem::transmute(x) };
            bytes.extend(x_bytes.iter());
        }

        // Even an empty string must be prefixed with `0x`.
        // Encodes each byte as a two hex digits.
        let hex_string = format!("0x{}", hex::encode(bytes));
        let hex_string_obj = self.heap.asc_new(hex_string.as_str());
        Ok(Some(RuntimeValue::from(hex_string_obj)))
    }

    /// function typeConversion.h256ToH160(h256: H256): H160
    fn h256_to_h160(&self, h256_ptr: AscPtr<AscH256>) -> Result<Option<RuntimeValue>, Trap> {
        let h256: H256 = self.heap.asc_get(h256_ptr);
        let h160 = H160::from(h256);
        let h160_obj: AscPtr<AscH160> = self.heap.asc_new(&h160);
        Ok(Some(RuntimeValue::from(h160_obj)))
    }

    /// function typeConversion.h160ToH256(h160: H160): H256
    fn h160_to_h256(&self, h160_ptr: AscPtr<AscH160>) -> Result<Option<RuntimeValue>, Trap> {
        let h160: H160 = self.heap.asc_get(h160_ptr);
        let h256 = H256::from(h160);
        let h256_obj: AscPtr<AscH256> = self.heap.asc_new(&h256);
        Ok(Some(RuntimeValue::from(h256_obj)))
    }

    /// function typeConversion.u256ToH160(u256: U256): H160
    fn u256_to_h160(&self, u256_ptr: AscPtr<AscU256>) -> Result<Option<RuntimeValue>, Trap> {
        let u256: U256 = self.heap.asc_get(u256_ptr);
        let h256 = H256::from(u256);
        let h160 = H160::from(h256);
        let h160_obj: AscPtr<AscH160> = self.heap.asc_new(&h160);
        Ok(Some(RuntimeValue::from(h160_obj)))
    }

    /// function typeConversion.u256ToH256(u256: U256): H256
    fn u256_to_h256(&self, u256_ptr: AscPtr<AscU256>) -> Result<Option<RuntimeValue>, Trap> {
        let u256: U256 = self.heap.asc_get(u256_ptr);
        let h256 = H256::from(u256);
        let h256_obj: AscPtr<AscH256> = self.heap.asc_new(&h256);
        Ok(Some(RuntimeValue::from(h256_obj)))
    }

    /// function typeConversion.stringToH160(s: String): H160
    fn string_to_h160(&self, str_ptr: AscPtr<AscString>) -> Result<Option<RuntimeValue>, Trap> {
        let s: String = self.heap.asc_get(str_ptr);
        let h160 = H160::from_str(s.as_str())
            .map_err(|e| host_error(format!("Failed to convert string to Address/H160: {}", e)))?;
        let h160_obj: AscPtr<AscH160> = self.heap.asc_new(&h160);
        Ok(Some(RuntimeValue::from(h160_obj)))
    }

    /// This works for both U256 and I256.
    /// function typeConversion.int256ToBigInt(int256: Uint64Array): BigInt
    fn int256_to_big_int(
        &self,
        int256_ptr: AscPtr<Uint64Array>,
    ) -> Result<Option<RuntimeValue>, Trap> {
        // Read as a U256 to use the convenient `to_little_endian` method.
        let int256: U256 = self.heap.asc_get(int256_ptr);
        let mut buffer = [0; 32];
        int256.to_little_endian(&mut buffer);
        let big_int_obj: AscPtr<BigInt> = self.heap.asc_new(&buffer[..]);
        Ok(Some(RuntimeValue::from(big_int_obj)))
    }

    /// function typeConversion.bigIntToInt256(i: BigInt): Uint64Array
    fn big_int_to_int256(&self, big_int_ptr: AscPtr<BigInt>) -> Result<Option<RuntimeValue>, Trap> {
        let bytes: Vec<u8> = self.heap.asc_get(big_int_ptr);
        let size = bytes.len();
        if size != 32 {
            Err(host_error(format!(
                "expected byte array of size 32, found size {}",
                size
            )))
        } else {
            let int256 = U256::from_little_endian(&*bytes);
            let int256_ptr: AscPtr<Uint64Array> = self.heap.asc_new(&int256);
            Ok(Some(RuntimeValue::from(int256_ptr)))
        }
    }

    /// Converts bytes to a hex string.
    /// References:
    /// https://godoc.org/github.com/ethereum/go-ethereum/common/hexutil#hdr-Encoding_Rules
    /// https://github.com/ethereum/web3.js/blob/f98fe1462625a6c865125fecc9cb6b414f0a5e83/packages/web3-utils/src/utils.js#L283
    ///
    /// function typeConversion.bytesToHex(bytes: Bytes): string
    fn bytes_to_hex(&self, bytes_ptr: AscPtr<Uint8Array>) -> Result<Option<RuntimeValue>, Trap> {
        let bytes: Vec<u8> = self.heap.asc_get(bytes_ptr);

        // Even an empty string must be prefixed with `0x`.
        // Encodes each byte as a two hex digits.
        let hex_string = format!("0x{}", hex::encode(bytes));
        let hex_string_obj = self.heap.asc_new(hex_string.as_str());

        Ok(Some(RuntimeValue::from(hex_string_obj)))
    }

    /// function json.fromBytes(bytes: Bytes): JSONValue
    fn json_from_bytes(&self, bytes_ptr: AscPtr<Uint8Array>) -> Result<Option<RuntimeValue>, Trap> {
        let bytes: Vec<u8> = self.heap.asc_get(bytes_ptr);
        let json: serde_json::Value =
            serde_json::from_reader(&*bytes).map_err(HostExternalsError)?;
        let json_obj = self.heap.asc_new(&json);
        Ok(Some(RuntimeValue::from(json_obj)))
    }

    /// function ipfs.cat(link: String): Bytes
    fn ipfs_cat(&self, link_ptr: AscPtr<AscString>) -> Result<Option<RuntimeValue>, Trap> {
        let link = self.heap.asc_get(link_ptr);
        let bytes = self.block_on(
            self.link_resolver
                .cat(&Link { link })
                .map_err(|e| HostExternalsError(e.to_string())),
        )?;
        let bytes_obj: AscPtr<Uint8Array> = self.heap.asc_new(&*bytes);
        Ok(Some(RuntimeValue::from(bytes_obj)))
    }

    /// Expects a decimal string.
    /// function json.toI64(json: String): i64
    fn json_to_i64(&self, json_ptr: AscPtr<AscString>) -> Result<Option<RuntimeValue>, Trap> {
        let json: String = self.heap.asc_get(json_ptr);
        let number = i64::from_str(&json)
            .map_err(|_| host_error(format!("JSON `{}` cannot be parsed as i64", json)))?;
        Ok(Some(RuntimeValue::from(number)))
    }

    /// Expects a decimal string.
    /// function json.toU64(json: String): u64
    fn json_to_u64(&self, json_ptr: AscPtr<AscString>) -> Result<Option<RuntimeValue>, Trap> {
        let json: String = self.heap.asc_get(json_ptr);
        let number = u64::from_str(&json)
            .map_err(|_| host_error(format!("JSON `{}` cannot be parsed as u64", json)))?;
        Ok(Some(RuntimeValue::from(number)))
    }

    /// Expects a decimal string.
    /// function json.toF64(json: String): f64
    fn json_to_f64(&self, json_ptr: AscPtr<AscString>) -> Result<Option<RuntimeValue>, Trap> {
        let json: String = self.heap.asc_get(json_ptr);
        let number = f64::from_str(&json)
            .map_err(|_| host_error(format!("JSON `{}` cannot be parsed as f64", json)))?;
        Ok(Some(RuntimeValue::from(F64::from(number))))
    }

    /// Expects a decimal string.
    /// function json.toBigInt(json: String): BigInt
    fn json_to_big_int(&self, json: AscPtr<AscString>) -> Result<Option<RuntimeValue>, Trap> {
        let json: String = self.heap.asc_get(json);
        let big_int = scalar::BigInt::from_str(&json)
            .map_err(|_| host_error(format!("JSON `{}` is not a decimal string", json)))?;
        let big_int_ptr: AscPtr<BigInt> = self.heap.asc_new(&*big_int.to_signed_bytes_le());
        Ok(Some(RuntimeValue::from(big_int_ptr)))
    }

    fn block_on<I: Send + 'static, E: Send + 'static>(
        &self,
        future: impl Future<Item = I, Error = E> + Send + 'static,
    ) -> Result<I, E> {
        let (return_sender, return_receiver) = oneshot::channel();
        self.task_sink
            .clone()
            .send(Box::new(future.then(|res| {
                return_sender.send(res).map_err(|_| unreachable!())
            }))).wait()
            .map_err(|_| panic!("task receiver dropped"))
            .unwrap();
        return_receiver.wait().expect("`return_sender` dropped")
    }
}

impl<T, L, S, U> Externals for HostExternals<T, L, S, U>
where
    T: EthereumAdapter,
    L: LinkResolver,
    S: Store + Send + Sync,
    U: Sink<SinkItem = Box<Future<Item = (), Error = ()> + Send>> + Clone,
{
    fn invoke_index(
        &mut self,
        index: usize,
        args: RuntimeArgs,
    ) -> Result<Option<RuntimeValue>, Trap> {
        match index {
            STORE_SET_FUNC_INDEX => self.store_set(
                args.nth_checked(0)?,
                args.nth_checked(1)?,
                args.nth_checked(2)?,
            ),
            STORE_GET_FUNC_INDEX => self.store_get(args.nth_checked(0)?, args.nth_checked(1)?),
            STORE_REMOVE_FUNC_INDEX => {
                self.store_remove(args.nth_checked(0)?, args.nth_checked(1)?)
            }
            ETHEREUM_CALL_FUNC_INDEX => self.ethereum_call(args.nth_checked(0)?),
            TYPE_CONVERSION_BYTES_TO_STRING_FUNC_INDEX => {
                self.convert_bytes_to_string(args.nth_checked(0)?)
            }
            TYPE_CONVERSION_BYTES_TO_HEX_FUNC_INDEX => self.bytes_to_hex(args.nth_checked(0)?),
            TYPE_CONVERSION_U64_ARRAY_TO_STRING_FUNC_INDEX => {
                self.u64_array_to_string(args.nth_checked(0)?)
            }
            TYPE_CONVERSION_U64_ARRAY_TO_HEX_FUNC_INDEX => {
                self.u64_array_to_hex(args.nth_checked(0)?)
            }
            TYPE_CONVERSION_H256_TO_H160_FUNC_INDEX => self.h256_to_h160(args.nth_checked(0)?),
            TYPE_CONVERSION_H160_TO_H256_FUNC_INDEX => self.h160_to_h256(args.nth_checked(0)?),
            TYPE_CONVERSION_U256_TO_H160_FUNC_INDEX => self.u256_to_h160(args.nth_checked(0)?),
            TYPE_CONVERSION_U256_TO_H256_FUNC_INDEX => self.u256_to_h256(args.nth_checked(0)?),
            TYPE_CONVERSION_STRING_TO_H160_FUNC_INDEX => self.string_to_h160(args.nth_checked(0)?),
            TYPE_CONVERSION_INT256_TO_BIG_INT_FUNC_INDEX => {
                self.int256_to_big_int(args.nth_checked(0)?)
            }
            JSON_FROM_BYTES_FUNC_INDEX => self.json_from_bytes(args.nth_checked(0)?),
            JSON_TO_I64_FUNC_INDEX => self.json_to_i64(args.nth_checked(0)?),
            JSON_TO_U64_FUNC_INDEX => self.json_to_u64(args.nth_checked(0)?),
            JSON_TO_F64_FUNC_INDEX => self.json_to_f64(args.nth_checked(0)?),
            JSON_TO_BIG_INT_FUNC_INDEX => self.json_to_big_int(args.nth_checked(0)?),
            IPFS_CAT_FUNC_INDEX => self.ipfs_cat(args.nth_checked(0)?),
            TYPE_CONVERSION_BIG_INT_FUNC_TO_INT256_INDEX => {
                self.big_int_to_int256(args.nth_checked(0)?)
            }
            _ => panic!("Unimplemented function at {}", index),
        }
    }
}

/// Env module resolver
pub struct EnvModuleResolver;

impl ModuleImportResolver for EnvModuleResolver {
    fn resolve_func(&self, field_name: &str, _signature: &Signature) -> Result<FuncRef, Error> {
        Ok(match field_name {
            "abort" => FuncInstance::alloc_host(
                Signature::new(
                    &[
                        ValueType::I32,
                        ValueType::I32,
                        ValueType::I32,
                        ValueType::I32,
                    ][..],
                    None,
                ),
                ABORT_FUNC_INDEX,
            ),
            _ => {
                return Err(Error::Instantiation(format!(
                    "Export '{}' not found",
                    field_name
                )));
            }
        })
    }
}

/// Store module resolver
pub struct StoreModuleResolver;

impl ModuleImportResolver for StoreModuleResolver {
    fn resolve_func(&self, field_name: &str, _signature: &Signature) -> Result<FuncRef, Error> {
        Ok(match field_name {
            "set" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32, ValueType::I32, ValueType::I32][..], None),
                STORE_SET_FUNC_INDEX,
            ),
            "remove" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32, ValueType::I32][..], None),
                STORE_REMOVE_FUNC_INDEX,
            ),
            "get" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32, ValueType::I32][..], Some(ValueType::I32)),
                STORE_GET_FUNC_INDEX,
            ),
            _ => {
                return Err(Error::Instantiation(format!(
                    "Export '{}' not found",
                    field_name
                )))
            }
        })
    }
}

/// Ethereum module resolver
pub struct EthereumModuleResolver;

impl ModuleImportResolver for EthereumModuleResolver {
    fn resolve_func(&self, field_name: &str, _signature: &Signature) -> Result<FuncRef, Error> {
        Ok(match field_name {
            "call" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                ETHEREUM_CALL_FUNC_INDEX,
            ),
            _ => {
                return Err(Error::Instantiation(format!(
                    "Export '{}' not found",
                    field_name
                )))
            }
        })
    }
}

/// Types conversion module resolver
pub struct TypeConversionModuleResolver;

impl ModuleImportResolver for TypeConversionModuleResolver {
    fn resolve_func(&self, field_name: &str, _signature: &Signature) -> Result<FuncRef, Error> {
        Ok(match field_name {
            "bytesToString" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                TYPE_CONVERSION_BYTES_TO_STRING_FUNC_INDEX,
            ),
            "bytesToHex" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                TYPE_CONVERSION_BYTES_TO_HEX_FUNC_INDEX,
            ),
            "u64ArrayToString" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                TYPE_CONVERSION_U64_ARRAY_TO_STRING_FUNC_INDEX,
            ),
            "u64ArrayToHex" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                TYPE_CONVERSION_U64_ARRAY_TO_HEX_FUNC_INDEX,
            ),
            "h256ToH160" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                TYPE_CONVERSION_H256_TO_H160_FUNC_INDEX,
            ),
            "h160ToH256" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                TYPE_CONVERSION_H160_TO_H256_FUNC_INDEX,
            ),
            "u256ToH160" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                TYPE_CONVERSION_U256_TO_H160_FUNC_INDEX,
            ),
            "u256ToH256" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                TYPE_CONVERSION_U256_TO_H256_FUNC_INDEX,
            ),
            "stringToH160" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                TYPE_CONVERSION_STRING_TO_H160_FUNC_INDEX,
            ),
            "int256ToBigInt" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                TYPE_CONVERSION_INT256_TO_BIG_INT_FUNC_INDEX,
            ),
            "bigIntToInt256" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                TYPE_CONVERSION_BIG_INT_FUNC_TO_INT256_INDEX,
            ),
            _ => {
                return Err(Error::Instantiation(format!(
                    "Export '{}' not found",
                    field_name
                )))
            }
        })
    }
}

struct JsonModuleResolver;

impl ModuleImportResolver for JsonModuleResolver {
    fn resolve_func(&self, field_name: &str, _signature: &Signature) -> Result<FuncRef, Error> {
        Ok(match field_name {
            "fromBytes" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                JSON_FROM_BYTES_FUNC_INDEX,
            ),
            "toI64" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I64)),
                JSON_TO_I64_FUNC_INDEX,
            ),
            "toU64" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I64)),
                JSON_TO_U64_FUNC_INDEX,
            ),
            "toF64" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::F64)),
                JSON_TO_F64_FUNC_INDEX,
            ),
            "toBigInt" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                JSON_TO_BIG_INT_FUNC_INDEX,
            ),
            _ => {
                return Err(Error::Instantiation(format!(
                    "Export '{}' not found",
                    field_name
                )))
            }
        })
    }
}

struct IpfsModuleResolver;

impl ModuleImportResolver for IpfsModuleResolver {
    fn resolve_func(&self, field_name: &str, _signature: &Signature) -> Result<FuncRef, Error> {
        Ok(match field_name {
            "cat" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32][..], Some(ValueType::I32)),
                IPFS_CAT_FUNC_INDEX,
            ),
            _ => {
                return Err(Error::Instantiation(format!(
                    "Export '{}' not found",
                    field_name
                )))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate failure;
    extern crate graph_mock;
    extern crate graphql_parser;
    extern crate ipfs_api;
    extern crate parity_wasm;

    use self::graphql_parser::schema::Document;
    use ethabi::{LogParam, Token};
    use ethereum_types::Address;
    use futures::sync::mpsc::{channel, Receiver};
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::iter::FromIterator;
    use std::sync::Mutex;

    use self::graph_mock::FakeStore;
    use graph::components::ethereum::*;
    use graph::components::store::*;
    use graph::components::subgraph::*;
    use graph::data::subgraph::*;
    use graph::util;

    use super::*;

    #[derive(Default)]
    struct MockEthereumAdapter {}

    impl EthereumAdapter for MockEthereumAdapter {
        fn contract_call(
            &mut self,
            _call: EthereumContractCall,
        ) -> Box<Future<Item = Vec<Token>, Error = EthereumContractCallError>> {
            unimplemented!()
        }
    }

    fn test_module(
        data_source: DataSource,
    ) -> (
        WasmiModule<
            MockEthereumAdapter,
            ipfs_api::IpfsClient,
            FakeStore,
            Sender<Box<Future<Item = (), Error = ()> + Send>>,
        >,
        Receiver<RuntimeHostEvent>,
    ) {
        let logger = slog::Logger::root(slog::Discard, o!());
        let (sender, receiver) = channel(100);
        let mock_ethereum_adapter = Arc::new(Mutex::new(MockEthereumAdapter::default()));
        let (task_sender, task_receiver) = channel(100);
        let mut runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.spawn(task_receiver.for_each(tokio::spawn));
        ::std::mem::forget(runtime);
        (
            WasmiModule::new(
                &logger,
                WasmiModuleConfig {
                    subgraph: mock_subgraph(),
                    data_source,
                    event_sink: sender,
                    ethereum_adapter: mock_ethereum_adapter,
                    link_resolver: Arc::new(ipfs_api::IpfsClient::default()),
                    store: Arc::new(Mutex::new(FakeStore)),
                },
                task_sender,
            ),
            receiver,
        )
    }

    fn mock_subgraph() -> SubgraphManifest {
        SubgraphManifest {
            id: String::from("example subgraph"),
            location: String::from("/path/to/example-subgraph.yaml"),
            spec_version: String::from("0.1.0"),
            schema: Schema {
                name: String::from("exampled name"),
                id: String::from("exampled id"),
                document: Document {
                    definitions: vec![],
                },
            },
            data_sources: vec![],
        }
    }

    fn mock_data_source(path: &str) -> DataSource {
        let runtime = parity_wasm::deserialize_file(path).expect("Failed to deserialize wasm");

        DataSource {
            kind: String::from("ethereum/contract"),
            name: String::from("example data source"),
            source: Source {
                address: String::from("0123123123"),
                abi: String::from("123123"),
            },
            mapping: Mapping {
                kind: String::from("ethereum/events"),
                api_version: String::from("0.1.0"),
                language: String::from("wasm/assemblyscript"),
                entities: vec![],
                abis: vec![],
                event_handlers: vec![],
                runtime,
            },
        }
    }

    #[test]
    fn call_invalid_event_handler_and_dont_crash() {
        // This test passing means the module doesn't crash when an invalid
        // event handler is called or when the event handler execution fails.

        let (mut module, _) = test_module(mock_data_source("wasm_test/example_event_handler.wasm"));

        // Create a mock Ethereum event
        let ethereum_event = EthereumEvent {
            address: Address::from("22843e74c59580b3eaf6c233fa67d8b7c561a835"),
            event_signature: util::ethereum::string_to_h256("ExampleEvent(string)"),
            block_hash: util::ethereum::string_to_h256("example block hash"),
            params: vec![LogParam {
                name: String::from("exampleParam"),
                value: Token::String(String::from("some data")),
            }],
            removed: false,
        };

        // Call a non-existent event handler in the test module; if the test hasn't
        // crashed until now, it means it survives Ethereum event handler errors
        assert_eq!(
            module.handle_ethereum_event("handleNonExistentExampleEvent", ethereum_event),
            ()
        );
    }

    #[test]
    fn call_event_handler_and_receive_store_event() {
        // Load the example_event_handler.wasm test module. All this module does
        // is implement an `handleExampleEvent` function that calls `store.set()`
        // with sample data taken from the event parameters.
        //
        // This test verifies that the event is delivered and the example data
        // is returned to the RuntimeHostEvent stream.

        // Load the module
        let (mut module, receiver) =
            test_module(mock_data_source("wasm_test/example_event_handler.wasm"));

        // Create a mock Ethereum event
        let ethereum_event = EthereumEvent {
            address: Address::from("22843e74c59580b3eaf6c233fa67d8b7c561a835"),
            event_signature: util::ethereum::string_to_h256("ExampleEvent(string)"),
            block_hash: util::ethereum::string_to_h256("example block hash"),
            params: vec![LogParam {
                name: String::from("exampleParam"),
                value: Token::String(String::from("some data")),
            }],
            removed: false,
        };

        // Call the event handler in the test module and pass the event to it
        module.handle_ethereum_event("handleExampleEvent", ethereum_event);

        // Expect a store set call to be made by the handler and a
        // RuntimeHostEvent::EntitySet event to be written to the event stream
        let work = receiver.take(1).into_future();
        let store_event = work
            .wait()
            .expect("No store event received from runtime")
            .0
            .expect("Store event must not be None");

        // Verify that this event matches what the test module is sending
        assert_eq!(
            store_event,
            RuntimeHostEvent::EntitySet(
                StoreKey {
                    subgraph: String::from("example subgraph"),
                    entity: String::from("ExampleEntity"),
                    id: String::from("example id"),
                },
                Entity::from(HashMap::from_iter(
                    vec![(String::from("exampleAttribute"), Value::from("some data"))].into_iter()
                )),
                EventSource::EthereumBlock(util::ethereum::string_to_h256("example block hash",)),
            )
        );
    }

    #[test]
    fn json_conversions() {
        // Load the module
        let (mut module, _) = test_module(mock_data_source("wasm_test/string_to_number.wasm"));

        // test u64 conversion
        let number = 9223372036850770800;
        let converted: u64 = module
            .module
            .invoke_export(
                "testToU64",
                &[RuntimeValue::from(module.heap.asc_new(&number.to_string()))],
                &mut module.externals,
            ).expect("call failed")
            .expect("call returned nothing")
            .try_into()
            .expect("call did not return I64");
        assert_eq!(number, converted);

        // test i64 conversion
        let number = -9223372036850770800;
        let converted: i64 = module
            .module
            .invoke_export(
                "testToI64",
                &[RuntimeValue::from(module.heap.asc_new(&number.to_string()))],
                &mut module.externals,
            ).expect("call failed")
            .expect("call returned nothing")
            .try_into()
            .expect("call did not return I64");
        assert_eq!(number, converted);

        // test f64 conversion
        let number = F64::from(-9223372036850770.92345034);
        let converted: F64 = module
            .module
            .invoke_export(
                "testToF64",
                &[RuntimeValue::from(
                    module.heap.asc_new(&number.to_float().to_string()),
                )],
                &mut module.externals,
            ).expect("call failed")
            .expect("call returned nothing")
            .try_into()
            .expect("call did not return F64");
        assert_eq!(number, converted);

        // test BigInt conversion
        let number = "-922337203685077092345034";
        let big_int_obj: AscPtr<BigInt> = module
            .module
            .invoke_export(
                "testToBigInt",
                &[RuntimeValue::from(module.heap.asc_new(number))],
                &mut module.externals,
            ).expect("call failed")
            .expect("call returned nothing")
            .try_into()
            .expect("call did not return pointer");
        let bytes: Vec<u8> = module.heap.asc_get(big_int_obj);
        assert_eq!(
            scalar::BigInt::from_str(number).unwrap(),
            scalar::BigInt::from_signed_bytes_le(&bytes)
        );
    }

    #[test]
    fn ipfs_cat() {
        let (mut module, _) = test_module(mock_data_source("wasm_test/ipfs_cat.wasm"));
        let ipfs = Arc::new(ipfs_api::IpfsClient::default());
        let hash = module
            .externals
            .block_on(ipfs.add(Cursor::new("42")))
            .unwrap()
            .hash;
        let converted: AscPtr<AscString> = module
            .module
            .invoke_export(
                "ipfsCat",
                &[RuntimeValue::from(module.heap.asc_new(&hash))],
                &mut module.externals,
            ).expect("call failed")
            .expect("call returned nothing")
            .try_into()
            .expect("call did not return pointer");
        let data: String = module.heap.asc_get(converted);
        assert_eq!(data, "42");
    }
}
