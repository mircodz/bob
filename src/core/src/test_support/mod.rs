pub(crate) mod agent;
pub(crate) mod files;
pub(crate) mod http;
pub(crate) mod workspace;

pub(crate) use workspace::TestDir;

use std::path::{Path, PathBuf};

impl TestDir {
    pub fn write(&self, relative: impl AsRef<Path>, content: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().expect("fixture file parent"))
            .expect("create fixture parents");
        std::fs::write(&path, content).expect("write fixture file");
        path
    }
}
