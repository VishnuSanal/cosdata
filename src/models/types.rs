use crate::distance::DistanceError;
use crate::distance::{
    cosine::CosineDistance, dotproduct::DotProductDistance, euclidean::EuclideanDistance,
    hamming::HammingDistance, DistanceFunction,
};
use crate::models::common::*;
use crate::models::identity_collections::*;
use crate::models::lazy_load::*;
use crate::models::versioning::VersionHash;
use crate::quantization::product::ProductQuantization;
use crate::quantization::scalar::ScalarQuantization;
use crate::quantization::{Quantization, StorageType};
use crate::storage::Storage;
use arcshift::ArcShift;
use dashmap::DashMap;
use lmdb::{Database, Environment};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::*;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};

pub type HNSWLevel = u8;
pub type FileOffset = u32;
pub type BytesToRead = u32;
pub type VersionId = u16;
pub type CosineSimilarity = f32;

pub type Item<T> = ArcShift<T>;

#[derive(Clone)]
pub struct Neighbour {
    pub node: LazyItem<MergedNode>,
    pub cosine_similarity: CosineSimilarity,
}

impl Identifiable for Neighbour {
    type Id = LazyItemId;

    fn get_id(&self) -> Self::Id {
        self.node.get_id()
    }
}

impl Identifiable for MergedNode {
    type Id = u64;

    fn get_id(&self) -> Self::Id {
        let mut prop_ref = self.prop.clone();
        let prop = prop_ref.get();
        let mut hasher = DefaultHasher::new();
        prop.hash(&mut hasher);
        hasher.finish()
    }
}

pub type PropPersistRef = (FileOffset, BytesToRead);
pub type NodeFileRef = FileOffset;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeProp {
    pub id: VectorId,
    pub value: Arc<Storage>,
    pub location: Option<PropPersistRef>,
}

impl Hash for NodeProp {
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        self.id.hash(state);
    }
}

#[derive(Debug, Clone, Hash)]
pub enum PropState {
    Ready(Arc<NodeProp>),
    Pending(PropPersistRef),
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub enum VectorId {
    Str(String),
    Int(i32),
}

#[derive(Clone)]
pub struct MergedNode {
    pub version_id: VersionId,
    pub hnsw_level: HNSWLevel,
    pub prop: Item<PropState>,
    pub neighbors: EagerLazyItemSet<MergedNode, f32>,
    pub parent: LazyItemRef<MergedNode>,
    pub child: LazyItemRef<MergedNode>,
    pub versions: LazyItemMap<MergedNode>,
    pub persist_flag: Arc<AtomicBool>,
}

#[derive(Debug)]
pub enum DistanceMetric {
    Cosine,
    Euclidean,
    Hamming,
    DotProduct,
}

impl DistanceFunction for DistanceMetric {
    fn calculate(&self, x: &Storage, y: &Storage) -> Result<f32, DistanceError> {
        match self {
            Self::Cosine => CosineDistance.calculate(x, y),
            Self::Euclidean => EuclideanDistance.calculate(x, y),
            Self::Hamming => HammingDistance.calculate(x, y),
            Self::DotProduct => DotProductDistance.calculate(x, y),
        }
    }
}

#[derive(Debug)]
pub enum QuantizationMetric {
    Scalar,
    Product(ProductQuantization),
}

impl Quantization for QuantizationMetric {
    fn quantize(&self, vector: &[f32], storage_type: StorageType) -> Storage {
        match self {
            Self::Scalar => ScalarQuantization.quantize(vector, storage_type),
            Self::Product(product) => product.quantize(vector, storage_type),
        }
    }

    fn train(
        &mut self,
        vectors: &[Vec<f32>],
    ) -> Result<(), crate::quantization::QuantizationError> {
        match self {
            Self::Scalar => ScalarQuantization.train(vectors),
            Self::Product(product) => product.train(vectors),
        }
    }
}

impl MergedNode {
    pub fn new(version_id: VersionId, hnsw_level: HNSWLevel) -> Self {
        MergedNode {
            version_id,
            hnsw_level,
            prop: Item::new(PropState::Pending((0, 0))),
            neighbors: EagerLazyItemSet::new(),
            parent: LazyItemRef::new_invalid(),
            child: LazyItemRef::new_invalid(),
            versions: LazyItemMap::new(),
            persist_flag: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn add_ready_neighbor(&self, neighbor: LazyItem<MergedNode>, cosine_similarity: f32) {
        self.neighbors
            .insert(EagerLazyItem(cosine_similarity, neighbor));
    }

    pub fn set_parent(&self, parent: LazyItem<MergedNode>) {
        let mut arc = self.parent.item.clone();
        arc.update(parent);
    }

    pub fn set_child(&self, child: LazyItem<MergedNode>) {
        let mut arc = self.child.item.clone();
        arc.update(child);
    }

    pub fn add_ready_neighbors(&self, neighbors_list: Vec<(LazyItem<MergedNode>, f32)>) {
        for (neighbor, cosine_similarity) in neighbors_list {
            self.add_ready_neighbor(neighbor, cosine_similarity);
        }
    }

    pub fn get_neighbors(&self) -> EagerLazyItemSet<MergedNode, f32> {
        self.neighbors.clone()
    }

    // pub fn set_neighbors(&self, new_neighbors: IdentitySet<EagerLazyItem<MergedNode, f32>>) {
    //     let mut arc = self.neighbors.items.clone();
    //     arc.update(new_neighbors);
    // }

    pub fn add_version(&self, version: Item<MergedNode>) {
        let lazy_item = LazyItem::from_item(version);
        // TODO: look at the id
        self.versions.insert(IdentityMapKey::Int(0), lazy_item);
    }

    pub fn get_versions(&self) -> LazyItemMap<MergedNode> {
        self.versions.clone()
    }

    pub fn get_parent(&self) -> LazyItemRef<MergedNode> {
        self.parent.clone()
    }

    pub fn get_child(&self) -> LazyItemRef<MergedNode> {
        self.child.clone()
    }

    pub fn set_prop_location(&self, new_location: PropPersistRef) {
        let mut arc = self.prop.clone();
        arc.update(PropState::Pending(new_location));
    }

    pub fn get_prop_location(&self) -> Option<PropPersistRef> {
        let mut arc = self.prop.clone();
        match arc.get() {
            PropState::Ready(ref node_prop) => node_prop.location,
            PropState::Pending(location) => Some(*location),
        }
    }

    pub fn get_prop(&self) -> PropState {
        let mut arc = self.prop.clone();
        arc.get().clone()
    }

    pub fn set_prop_pending(&self, prop_ref: PropPersistRef) {
        let mut arc = self.prop.clone();
        arc.update(PropState::Pending(prop_ref));
    }

    pub fn set_prop_ready(&self, node_prop: Arc<NodeProp>) {
        let mut arc = self.prop.clone();
        arc.update(PropState::Ready(node_prop));
    }
}

impl SyncPersist for MergedNode {
    fn set_persistence(&self, flag: bool) {
        self.persist_flag.store(flag, Ordering::Relaxed);
    }

    fn needs_persistence(&self) -> bool {
        self.persist_flag.load(Ordering::Relaxed)
    }
}

// Implementing the std::fmt::Display trait for VectorId
impl fmt::Display for VectorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VectorId::Str(s) => write!(f, "{}", s),
            VectorId::Int(i) => write!(f, "{}", i),
        }
    }
}

impl fmt::Display for MergedNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "MergedNode {{")?;
        writeln!(f, "  version_id: {},", self.version_id)?;
        writeln!(f, "  hnsw_level: {},", self.hnsw_level)?;

        // Display PropState
        write!(f, "  prop: ")?;
        let mut prop_arc = self.prop.clone();
        match prop_arc.get() {
            PropState::Ready(node_prop) => writeln!(f, "Ready {{ id: {} }}", node_prop.id)?,
            PropState::Pending(_) => writeln!(f, "Pending")?,
        }
        // Display number of neighbors
        writeln!(f, "  neighbors: {} items,", self.neighbors.len())?;

        // Display parent and child status
        writeln!(
            f,
            "  parent: {}",
            if self.parent.is_valid() {
                "Valid"
            } else {
                "Invalid"
            }
        )?;
        writeln!(
            f,
            "  child: {}",
            if self.child.is_valid() {
                "Valid"
            } else {
                "Invalid"
            }
        )?;

        // Display number of versions
        writeln!(f, "  versions: {} items,", self.versions.len())?;

        // Display persist flag
        writeln!(
            f,
            "  persist_flag: {}",
            self.persist_flag.load(std::sync::atomic::Ordering::Relaxed)
        )?;

        write!(f, "}}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VectorQt {
    UnsignedByte {
        mag: u32,
        quant_vec: Vec<u8>,
    },
    SubByte {
        mag: u32,
        quant_vec: Vec<Vec<u8>>,
        resolution: u8,
    },
}

impl VectorQt {
    pub fn unsigned_byte(vec: &[f32]) -> Self {
        let quant_vec = simp_quant(vec);
        let mag = mag_square_u8(&quant_vec);
        Self::UnsignedByte { mag, quant_vec }
    }

    pub fn sub_byte(vec: &[f32], resolution: u8) -> Self {
        let quant_vec = quantize_to_u8_bits(vec, resolution);
        let mag = 0; //implement a proper magnitude calculation
        Self::SubByte {
            mag,
            quant_vec,
            resolution,
        }
    }
}

pub type SizeBytes = u32;

// needed to flatten and get uniques
pub type ExecQueueUpdate = Item<Vec<Item<LazyItem<MergedNode>>>>;

#[derive(Debug, Clone)]
pub struct MetaDb {
    pub env: Arc<Environment>,
    pub metadata_db: Arc<Database>,
    pub embeddings_db: Arc<Database>,
}

#[derive(Clone)]
pub struct VectorStore {
    pub exec_queue_nodes: ExecQueueUpdate,
    pub max_cache_level: u8,
    pub database_name: String,
    pub root_vec: LazyItemRef<MergedNode>,
    pub levels_prob: Arc<Vec<(f64, i32)>>,
    pub quant_dim: usize,
    pub prop_file: Arc<File>,
    pub lmdb: MetaDb,
    pub current_version: Item<Option<VersionHash>>,
    pub current_open_transaction: Item<Option<VersionHash>>,
    pub quantization_metric: Arc<QuantizationMetric>,
    pub distance_metric: Arc<DistanceMetric>,
    pub storage_type: StorageType,
}

impl VectorStore {
    pub fn new(
        exec_queue_nodes: ExecQueueUpdate,
        max_cache_level: u8,
        database_name: String,
        root_vec: LazyItemRef<MergedNode>,
        levels_prob: Arc<Vec<(f64, i32)>>,
        quant_dim: usize,
        prop_file: Arc<File>,
        lmdb: MetaDb,
        current_version: Item<Option<VersionHash>>,
        quantization_metric: Arc<QuantizationMetric>,
        distance_metric: Arc<DistanceMetric>,
        storage_type: StorageType,
    ) -> Self {
        VectorStore {
            exec_queue_nodes,
            max_cache_level,
            database_name,
            root_vec,
            levels_prob,
            quant_dim,
            prop_file,
            lmdb,
            current_version,
            current_open_transaction: Item::new(None),
            quantization_metric,
            distance_metric,
            storage_type,
        }
    }
    // Get method
    pub fn get_current_version(&self) -> Option<VersionHash> {
        let mut arc = self.current_version.clone();
        arc.get().clone()
    }

    // Set method
    pub fn set_current_version(&self, new_version: Option<VersionHash>) {
        let mut arc = self.current_version.clone();
        arc.update(new_version);
    }
}
#[derive(Debug, Clone, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, PartialEq)]
pub struct VectorEmbedding {
    pub raw_vec: Arc<Storage>,
    pub hash_vec: VectorId,
}

type VectorStoreMap = DashMap<String, Arc<VectorStore>>;
type UserDataCache = DashMap<String, (String, i32, i32, std::time::SystemTime, Vec<String>)>;

// Define the AppEnv struct
pub struct AppEnv {
    pub user_data_cache: UserDataCache,
    pub vector_store_map: VectorStoreMap,
    pub persist: Arc<Environment>,
}

static AIN_ENV: OnceLock<Result<Arc<AppEnv>, WaCustomError>> = OnceLock::new();

pub fn get_app_env() -> Result<Arc<AppEnv>, WaCustomError> {
    AIN_ENV
        .get_or_init(|| {
            let path = Path::new("./_mdb"); // TODO: prefix the customer & database name

            // Ensure the directory exists
            create_dir_all(&path).map_err(|e| WaCustomError::DatabaseError(e.to_string()))?;
            // Initialize the environment
            let env = Environment::new()
                .set_max_dbs(2)
                .set_map_size(10485760) // Set the maximum size of the database to 10MB
                .open(&path)
                .map_err(|e| WaCustomError::DatabaseError(e.to_string()))?;

            Ok(Arc::new(AppEnv {
                user_data_cache: DashMap::new(),
                vector_store_map: DashMap::new(),
                persist: Arc::new(env),
            }))
        })
        .clone()
}
