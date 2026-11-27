//! A repair page holds neither a global checkout nor a session connection while
//! waiting for the other. Only schema migration is allowed to backfill history.
use super::*;
use std::fs::File;
use std::sync::{atomic::Ordering, mpsc};

const PAGE: usize = 8;
const PASS_BUDGET: Duration = Duration::from_millis(200);
const INTERVAL: Duration = Duration::from_secs(2);

impl Stores {
    pub(super) fn start_projection_repair(&self) {
        self.root.repair.get_or_init(|| {
            if self.root.upgraded_directory {
                self.migrate_directory_summaries();
            }
            self.try_repair_page();
            let root = Arc::downgrade(&self.root);
            let (stop, rx) = mpsc::channel();
            std::thread::spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(INTERVAL) {
                    let Some(root) = root.upgrade() else { break };
                    Stores { root, fault: None, selected: None, router: None }.try_repair_page();
                }
            });
            stop
        });
    }

    // Only the process which upgraded G performs this one-time backfill.
    // If interrupted, the ordinary rotating pass discovers remaining old shards.
    fn migrate_directory_summaries(&self) {
        let old_rows = self.global().read(|c| {
            let mut stmt = c.prepare(
                "SELECT id,storage_key,incarnation FROM conversations WHERE content_revision=-1",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        });
        match old_rows {
            Ok(rows) => {
                for (id, key, incarnation) in rows {
                    let session = SessionDb {
                        stores: self.clone(),
                        id,
                        key,
                        incarnation,
                    };
                    if let Err(error) = session.upgrade_for_repair() {
                        tracing::debug!(%error,"Session migration postponed");
                    }
                }
            }
            Err(error) => tracing::debug!(%error,"Directory migration postponed"),
        }
    }

    fn try_repair_page(&self) {
        if let Err(error) = self.repair_page() {
            tracing::debug!(%error, "Directory projection pass postponed");
        }
    }

    fn repair_page(&self) -> Result<()> {
        let start = Instant::now();
        let mut cursor = self.root.repair_cursor.lock().unwrap();
        let entries = self
            .global()
            .checkout(OpenFlags::SQLITE_OPEN_READ_ONLY, true, |c| {
                let mut stmt = c.prepare(
                    "SELECT id,storage_key,incarnation,content_revision FROM conversations
                WHERE storage_key>?1 ORDER BY storage_key LIMIT ?2",
                )?;
                let rows = stmt.query_map(rusqlite::params![*cursor, PAGE], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })?;
        let count = entries.len();
        for (id, key, incarnation, revision) in entries {
            *cursor = key.clone();
            if storage_key(&id)? == key {
                let session = SessionDb {
                    stores: self.clone(),
                    id,
                    key,
                    incarnation,
                };
                if let Err(error) = session.repair_background(revision) {
                    tracing::debug!(%error,session=%session.id,"Directory projection pending");
                }
            }
            if start.elapsed() >= PASS_BUDGET {
                return Ok(());
            }
        }
        if count < PAGE {
            cursor.clear();
        }
        Ok(())
    }
}

enum SummaryCheck {
    Current,
    Upgrade,
    Publish(Projection),
}

impl SessionDb {
    fn repair_locks(&self, exclusive: bool) -> Result<Option<(File, File)>> {
        let dir = self.stores.layout().lock_path("session");
        let open = |p: PathBuf| File::options().read(true).write(true).open(p);
        let namespace = open(dir.join(".directory"))?;
        if namespace.try_lock_shared().is_err() {
            return Ok(None);
        }
        ensure!(
            std::fs::read_to_string(dir.join(".initialized"))? == self.stores.global().uuid,
            "Lifecycle namespace identity changed"
        );
        let key = open(dir.join(&self.key))?;
        let locked = if exclusive {
            key.try_lock()
        } else {
            key.try_lock_shared()
        };
        Ok(locked.ok().map(|_| (namespace, key)))
    }

    fn upgrade_for_repair(&self) -> Result<()> {
        ensure!(storage_key(&self.id)? == self.key, "Invalid directory key");
        let Some(_locks) = self.repair_locks(true)? else {
            return Ok(());
        };
        let Some(_permit) = Permits::try_one() else {
            return Ok(());
        };
        self.check_directory()?;
        let path = self.stores.sessions().join(&self.key).join("session.db");
        if layout::needs_upgrade(&path, SESSION)? {
            let c = layout::open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            verify_manifest(
                &c,
                self.stores.global().store_uuid(),
                &self.id,
                &self.incarnation,
            )?;
            drop(c);
            layout::upgrade(&path, SESSION)?;
        }
        Ok(())
    }

    fn repair_background(&self, published: i64) -> Result<()> {
        let Some(_locks) = self.repair_locks(false)? else {
            return Ok(());
        };
        match self.summary_under_lock(published)? {
            SummaryCheck::Current => Ok(()),
            SummaryCheck::Publish(p) => {
                self.stores
                    .global()
                    .project(&self.id, &self.incarnation, &p)
            }
            SummaryCheck::Upgrade => {
                drop(_locks);
                // Backfill outside the maintenance thread, at most one shard at a time.
                if !self.stores.root.migrating.swap(true, Ordering::AcqRel) {
                    let session = self.clone();
                    std::thread::spawn(move || {
                        if let Err(error) = session.upgrade_for_repair() {
                            tracing::debug!(%error, "Session migration postponed");
                        }
                        session
                            .stores
                            .root
                            .migrating
                            .store(false, Ordering::Release);
                    });
                }
                Ok(())
            }
        }
    }

    fn summary_under_lock(&self, published: i64) -> Result<SummaryCheck> {
        let Some(_permit) = Permits::try_one() else {
            return Ok(SummaryCheck::Current);
        };
        let path = self.stores.sessions().join(&self.key).join("session.db");
        let mut c = layout::open_timed(&path, OpenFlags::SQLITE_OPEN_READ_ONLY, 0)?;
        let tx = c.transaction()?;
        verify_manifest(
            &tx,
            self.stores.global().store_uuid(),
            &self.id,
            &self.incarnation,
        )?;
        let version: i32 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if (1..layout::SESSION_VERSION).contains(&version) {
            return Ok(SummaryCheck::Upgrade);
        }
        let (_, external_edit) = layout::session_header(&tx)?;
        ensure!(external_edit.is_none(), "Unvalidated external history edit");
        let revision: i64 = tx.query_row(
            "SELECT content_revision FROM manifest WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if revision <= published {
            return Ok(SummaryCheck::Current);
        }
        layout::verify_structure(&tx, SESSION, layout::SESSION_VERSION)?;
        Ok(SummaryCheck::Publish(history::projection(&tx)?))
    }

    pub(super) fn try_project_current(&self, revision: i64) {
        let result = (|| {
            let published = self.stores.global().read(|c| {
                Ok(c.query_row(
                    "SELECT content_revision FROM conversations WHERE id=?1 AND incarnation=?2",
                    [&self.id, &self.incarnation],
                    |r| r.get::<_, i64>(0),
                )?)
            })?;
            if revision <= published {
                return Ok(());
            }
            if let SummaryCheck::Publish(p) = self.summary_under_lock(published)? {
                self.stores
                    .global()
                    .project(&self.id, &self.incarnation, &p)?;
            }
            Ok::<(), anyhow::Error>(())
        })();
        if let Err(error) = result {
            tracing::debug!(%error,session=%self.id,"Directory projection pending");
        }
    }
}
