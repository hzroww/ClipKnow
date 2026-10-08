//! 测试用的真文件库。
//!
//! 内存库（`SqliteStore::in_memory`）每开一条连接就是一个**新的空库**，
//! 测不了「两条连接看到同一份数据」——并发落库、工具在另一条线程上开自己的
//! 连接，这两类测试都需要真文件。

use crate::content::model::new_id;
use crate::store::sqlite::SqliteStore;

/// 一个已经迁移好的真文件库，测试结束自动删掉（连 -wal / -shm 一起）。
pub struct TempDb(std::path::PathBuf);

impl TempDb {
    pub fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "clipknow-test-{tag}-{}-{}.db",
            std::process::id(),
            new_id()
        ));
        SqliteStore::open_and_migrate(p.to_str().unwrap()).unwrap();
        TempDb(p)
    }

    pub fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path()));
        }
    }
}
