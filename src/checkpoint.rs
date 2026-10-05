use crate::{
    canonical::sha256,
    error::{require, Error, Result},
    manifest::{external_path, read_file},
};
use fs2::FileExt;
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub struct Checkpoint {
    dir: PathBuf,
    _lock: File,
    sequence: usize,
    pending: Vec<Value>,
    pub run_id: Option<String>,
}
/// Checkpoint files may hold account mappings; they are owner-only (0600).
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
/// Opens (or creates, 0700) a checkpoint directory whose parent is outside the
/// repository, rejecting symlinks, and takes its exclusive writer lock.
pub fn open_locked_dir(path: &Path, resume: bool) -> Result<(PathBuf, File)> {
    let parent = external_path(
        path.parent()
            .ok_or_else(|| Error::new("checkpoint_path_invalid"))?,
    )?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::new("checkpoint_path_invalid"))?;
    let dir = parent.join(name);
    require(
        !fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()),
        "checkpoint_symlink_forbidden",
    )?;
    if !dir.exists() {
        require(!resume, "checkpoint_resume_missing")?;
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&dir)
            .map_err(|_| Error::new("checkpoint_create_failed"))?;
    } else {
        require(resume, "checkpoint_exists_use_resume")?;
    }
    let lockpath = dir.join("writer.lock");
    require(
        !fs::symlink_metadata(&lockpath).is_ok_and(|m| m.file_type().is_symlink()),
        "checkpoint_symlink_forbidden",
    )?;
    let lock = private_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lockpath)
        .map_err(|_| Error::new("checkpoint_lock_failed"))?;
    lock.try_lock_exclusive()
        .map_err(|_| Error::new("checkpoint_in_use"))?;
    Ok((dir, lock))
}
pub fn atomic_write(path: &Path, value: &Value) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new("checkpoint_path_invalid"))?;
    let tmp = parent.join(format!(".write-{}", std::process::id()));
    let mut file = private_options()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|_| Error::new("checkpoint_temp_failed"))?;
    let result = (|| {
        let bytes = serde_json::to_vec(value).map_err(|_| Error::new("checkpoint_json_invalid"))?;
        file.write_all(&bytes)
            .map_err(|_| Error::new("checkpoint_write_failed"))?;
        file.sync_all()
            .map_err(|_| Error::new("checkpoint_sync_failed"))?;
        fs::rename(&tmp, path).map_err(|_| Error::new("checkpoint_rename_failed"))?;
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| Error::new("checkpoint_directory_sync_failed"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
impl Checkpoint {
    pub fn open(path: &Path, identity: &Value, resume: bool) -> Result<Self> {
        let (dir, lock) = open_locked_dir(path, resume)?;
        let meta = dir.join("run.json");
        let mut run_id = None;
        if meta.exists() {
            let saved: Value = serde_json::from_slice(&read_file(&meta, 1024 * 1024)?)
                .map_err(|_| Error::new("checkpoint_corrupt"))?;
            require(
                saved["identity"] == *identity,
                "checkpoint_identity_changed",
            )?;
            run_id = saved["runId"].as_str().map(str::to_owned);
        } else {
            atomic_write(&meta, &json!({"identity":identity,"runId":null}))?;
        }
        let mut batches = fs::read_dir(&dir)
            .map_err(|_| Error::new("checkpoint_read_failed"))?
            .filter_map(|x| x.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("batch-"))
            })
            .collect::<Vec<_>>();
        batches.sort();
        for (index, path) in batches.iter().enumerate() {
            require(
                path.file_name()
                    .is_some_and(|n| n == format!("batch-{:06}.json", index + 1).as_str()),
                "checkpoint_sequence_gap",
            )?;
            let batch: Value = serde_json::from_slice(&read_file(path, 1024 * 1024)?)
                .map_err(|_| Error::new("checkpoint_corrupt"))?;
            require(
                batch["checksum"] == sha256(batch["entries"].to_string().as_bytes())
                    && batch["entries"].as_array().is_some_and(|a| a.len() <= 25),
                "checkpoint_checksum_mismatch",
            )?;
        }
        Ok(Self {
            dir,
            _lock: lock,
            sequence: batches.len(),
            pending: Vec::new(),
            run_id,
        })
    }
    pub fn set_run(&mut self, identity: &Value, run: &str) -> Result<()> {
        require(
            self.run_id.as_ref().is_none_or(|s| s == run),
            "checkpoint_run_changed",
        )?;
        atomic_write(
            &self.dir.join("run.json"),
            &json!({"identity":identity,"runId":run}),
        )?;
        self.run_id = Some(run.into());
        Ok(())
    }
    pub fn record(&mut self, value: &Value) -> Result<()> {
        // Body/source URLs/ingress URLs never enter the on-disk journal.
        let safe = json!({"type":value["type"],"sourceKey":value["sourceKey"],"targetDocumentId":value["targetDocumentId"],"operation":value["operation"]});
        self.pending.push(safe);
        if self.pending.len() >= 25 {
            self.flush()?;
        }
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let entries = serde_json::to_value(&self.pending)
            .map_err(|_| Error::new("checkpoint_json_invalid"))?;
        atomic_write(
            &self
                .dir
                .join(format!("batch-{:06}.json", self.sequence + 1)),
            &json!({"checksum":sha256(entries.to_string().as_bytes()),"entries":entries}),
        )?;
        self.sequence += 1;
        self.pending.clear();
        Ok(())
    }
}
