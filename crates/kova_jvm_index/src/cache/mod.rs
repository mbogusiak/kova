//! Persistent binary workspace index cache (`KIDX`) and shared `.jar` shards (`KJAR`).

pub mod codec;
pub mod revalidate;
pub mod store;

pub use codec::{
    ArtifactFingerprint, CacheLoadOutcome, CacheMissReason, CachedJarShard, CachedSourceEntry,
    CachedWorkspaceSnapshot, FileFingerprint, HEADER_SIZE, MAGIC_JAR_SHARD, MAGIC_WORKSPACE,
    MAX_CACHE_FILE_BYTES, SCHEMA_VERSION, ScanConfigFingerprint, decode_jar_shard,
    decode_workspace_snapshot, encode_jar_shard, encode_workspace_snapshot,
};
pub use revalidate::{
    IndexDelta, RevalidatedIndexResult, compute_index_delta, load_and_revalidate,
};
pub use store::{JvmCacheLayout, JvmIndexCacheStore};
