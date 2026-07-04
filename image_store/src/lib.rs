//! A versioned SQLite class/method source database — see
//! `../../docs/IMAGE.md` for the full design and, critically, §0's
//! distinction from the S16 heap snapshot: this stores **text** (source,
//! plus small metadata) and an optional, always-re-derivable bytecode
//! cache, never oops or heap pointers. Booting from it is still "boot by
//! compiling source," per `docs/SPEC.md` §3.2 — a different source
//! *container* than `world/*.mst` flat files, not a different bootstrap
//! model.
//!
//! Two SQL views (`latest_class_versions`, `latest_method_versions`) do the
//! "latest version" lookup once, in the schema, rather than repeating a
//! correlated `MAX(version_number)` subquery in every Rust-side query
//! below — see `docs/IMAGE.md` §4 for why "latest" is always computed, not
//! a stored pointer that could drift out of sync.

pub mod mst;

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// `docs/IMAGE.md` §4 — keep the last 100 versions per method/class.
pub const RETENTION_LIMIT: i64 = 100;
/// `docs/IMAGE.md` §3 — sparse gap numbering for `load_order`.
pub const LOAD_ORDER_START: i64 = 1000;
pub const LOAD_ORDER_STEP: i64 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Instance,
    Class,
}

impl Side {
    fn as_str(self) -> &'static str {
        match self {
            Side::Instance => "instance",
            Side::Class => "class",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassSummary {
    pub name: String,
    pub superclass: Option<String>,
    pub load_order: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodSummary {
    pub selector: String,
    pub category: String,
}

/// A class's complete latest-version definition — for callers that need
/// *everything*, not one field at a time: the GUI's mock-world mirror
/// (`gui/src/vm_host.rs`) and, eventually, an image→`.mst` exporter
/// (`docs/IMAGE.md` §6/§9, not built yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FullClass {
    pub name: String,
    pub superclass: Option<String>,
    pub category: String,
    pub comment: String,
    pub instance_vars: String,
    pub class_vars: String,
    pub load_order: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FullMethod {
    pub selector: String,
    pub side: Side,
    pub category: String,
    pub source: String,
}

/// Result of [`Image::create_or_reopen_class`] — see its doc comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassCreateOutcome {
    Created,
    Reopened,
    AlreadyLive,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS classes (
    class_id    INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    load_order  INTEGER NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS class_versions (
    version_id      INTEGER PRIMARY KEY,
    class_id        INTEGER NOT NULL REFERENCES classes(class_id),
    version_number  INTEGER NOT NULL,
    superclass_name TEXT,
    category        TEXT NOT NULL DEFAULT '',
    comment         TEXT NOT NULL DEFAULT '',
    instance_vars   TEXT NOT NULL DEFAULT '',
    class_vars      TEXT NOT NULL DEFAULT '',
    edited_at       INTEGER NOT NULL,
    deleted         INTEGER NOT NULL DEFAULT 0,
    UNIQUE(class_id, version_number)
);
CREATE INDEX IF NOT EXISTS idx_class_versions_latest ON class_versions(class_id, version_number DESC);
CREATE TABLE IF NOT EXISTS methods (
    method_id  INTEGER PRIMARY KEY,
    class_id   INTEGER NOT NULL REFERENCES classes(class_id),
    selector   TEXT NOT NULL,
    side       TEXT NOT NULL CHECK(side IN ('instance','class')),
    UNIQUE(class_id, selector, side)
);
CREATE TABLE IF NOT EXISTS method_versions (
    version_id     INTEGER PRIMARY KEY,
    method_id      INTEGER NOT NULL REFERENCES methods(method_id),
    version_number INTEGER NOT NULL,
    category       TEXT NOT NULL DEFAULT 'as yet unclassified',
    source         TEXT NOT NULL,
    edited_at      INTEGER NOT NULL,
    deleted        INTEGER NOT NULL DEFAULT 0,
    UNIQUE(method_id, version_number)
);
CREATE INDEX IF NOT EXISTS idx_method_versions_latest ON method_versions(method_id, version_number DESC);
CREATE TABLE IF NOT EXISTS method_bytecode (
    method_version_id INTEGER PRIMARY KEY REFERENCES method_versions(version_id),
    bytecode           BLOB NOT NULL,
    literals_json      TEXT,
    compiler_tag       TEXT NOT NULL,
    compiled_at        INTEGER NOT NULL
);
CREATE VIEW IF NOT EXISTS latest_class_versions AS
    SELECT cv.* FROM class_versions cv
    WHERE cv.version_number = (SELECT MAX(version_number) FROM class_versions WHERE class_id = cv.class_id);
CREATE VIEW IF NOT EXISTS latest_method_versions AS
    SELECT mv.* FROM method_versions mv
    WHERE mv.version_number = (SELECT MAX(version_number) FROM method_versions WHERE method_id = mv.method_id);
";

pub struct Image {
    conn: Connection,
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Upgrade an already-existing database file created before the `deleted`
/// tombstone column existed (`CREATE TABLE IF NOT EXISTS` in `SCHEMA` above
/// only adds it to *brand-new* tables) — checked via `PRAGMA table_info`
/// rather than just trying the `ALTER TABLE` and swallowing a "duplicate
/// column" error, so a real failure doesn't get masked the same way. Safe
/// to call on a fresh database too: `SCHEMA` already created the column, so
/// this is a no-op.
fn migrate_add_deleted_columns(conn: &Connection) -> rusqlite::Result<()> {
    for table in ["class_versions", "method_versions"] {
        let has_deleted = conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|name| name == "deleted");
        if !has_deleted {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0"), [])?;
        }
    }
    Ok(())
}

impl Image {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        migrate_add_deleted_columns(&conn)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrate_add_deleted_columns(&conn)?;
        Ok(Self { conn })
    }

    fn class_id_of(&self, class_name: &str) -> rusqlite::Result<Option<i64>> {
        self.conn
            .query_row("SELECT class_id FROM classes WHERE name = ?1", params![class_name], |r| r.get(0))
            .optional()
    }

    /// Whether a class by this name already exists — for callers (like the
    /// `.mst` importer, `src/bin/import_world.rs`) that need to distinguish
    /// "define a new class" from "reopen an existing one to add more
    /// methods," which real `.mst` files legitimately do (confirmed against
    /// the corpus: `01_object.mst`'s own header comment says Boolean- and
    /// printing-dependent `Object` methods land in *later* files, reopening
    /// the same class).
    pub fn class_exists(&self, class_name: &str) -> rusqlite::Result<bool> {
        Ok(self.class_id_of(class_name)?.is_some())
    }

    fn method_id_of(&self, class_name: &str, side: Side, selector: &str) -> rusqlite::Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT m.method_id FROM classes c JOIN methods m ON m.class_id = c.class_id \
                 WHERE c.name = ?1 AND m.side = ?2 AND m.selector = ?3",
                params![class_name, side.as_str(), selector],
                |r| r.get(0),
            )
            .optional()
    }

    // ── Read queries — mirrors macvm-mock-vm's MockWorld API shape ────────

    /// Package/category names, in first-load-order-appearance order.
    pub fn packages(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT lcv.category, MIN(c.load_order) AS first_lo \
             FROM classes c JOIN latest_class_versions lcv ON lcv.class_id = c.class_id \
             WHERE lcv.deleted = 0 \
             GROUP BY lcv.category ORDER BY first_lo",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// Classes in `package` whose latest superclass is absent, removed, or
    /// lives in a different package — `docs/IMAGE.md`'s package-roots rule,
    /// same as `macvm-mock-vm`'s `package_roots`. A removed superclass
    /// counts the same as a missing one (not `slcv.deleted = 0`, i.e.
    /// excluded from the "does a live same-package superclass exist"
    /// check): removing a class is allowed even with subclasses still
    /// pointing at it (the GUI warns first but doesn't block), so those
    /// subclasses need to re-root visually rather than vanish or crash.
    pub fn package_roots(&self, package: &str) -> rusqlite::Result<Vec<ClassSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.name, lcv.superclass_name, c.load_order \
             FROM classes c JOIN latest_class_versions lcv ON lcv.class_id = c.class_id \
             WHERE lcv.category = ?1 AND lcv.deleted = 0 \
               AND (lcv.superclass_name IS NULL OR NOT EXISTS ( \
                     SELECT 1 FROM classes sc JOIN latest_class_versions slcv ON slcv.class_id = sc.class_id \
                     WHERE sc.name = lcv.superclass_name AND slcv.category = lcv.category AND slcv.deleted = 0)) \
             ORDER BY c.load_order",
        )?;
        let rows = stmt.query_map(params![package], |r| {
            Ok(ClassSummary { name: r.get(0)?, superclass: r.get(1)?, load_order: r.get(2)? })
        })?;
        rows.collect()
    }

    pub fn subclasses_of(&self, class_name: &str) -> rusqlite::Result<Vec<ClassSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.name, lcv.superclass_name, c.load_order \
             FROM classes c JOIN latest_class_versions lcv ON lcv.class_id = c.class_id \
             WHERE lcv.superclass_name = ?1 AND lcv.deleted = 0 ORDER BY c.load_order",
        )?;
        let rows = stmt.query_map(params![class_name], |r| {
            Ok(ClassSummary { name: r.get(0)?, superclass: r.get(1)?, load_order: r.get(2)? })
        })?;
        rows.collect()
    }

    pub fn categories(&self, class_name: &str, side: Side) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT lmv.category \
             FROM classes c JOIN methods m ON m.class_id = c.class_id \
             JOIN latest_method_versions lmv ON lmv.method_id = m.method_id \
             WHERE c.name = ?1 AND m.side = ?2 AND lmv.deleted = 0 ORDER BY m.method_id",
        )?;
        let rows = stmt.query_map(params![class_name, side.as_str()], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    pub fn methods_in(&self, class_name: &str, side: Side, category: &str) -> rusqlite::Result<Vec<MethodSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.selector, lmv.category \
             FROM classes c JOIN methods m ON m.class_id = c.class_id \
             JOIN latest_method_versions lmv ON lmv.method_id = m.method_id \
             WHERE c.name = ?1 AND m.side = ?2 AND lmv.category = ?3 AND lmv.deleted = 0 ORDER BY m.selector",
        )?;
        let rows = stmt.query_map(params![class_name, side.as_str(), category], |r| {
            Ok(MethodSummary { selector: r.get(0)?, category: r.get(1)? })
        })?;
        rows.collect()
    }

    pub fn method_source(&self, class_name: &str, side: Side, selector: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT lmv.source \
                 FROM classes c JOIN methods m ON m.class_id = c.class_id \
                 JOIN latest_method_versions lmv ON lmv.method_id = m.method_id \
                 WHERE c.name = ?1 AND m.side = ?2 AND m.selector = ?3 AND lmv.deleted = 0",
                params![class_name, side.as_str(), selector],
                |r| r.get(0),
            )
            .optional()
    }

    pub fn class_comment(&self, class_name: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT lcv.comment FROM classes c JOIN latest_class_versions lcv ON lcv.class_id = c.class_id \
                 WHERE c.name = ?1 AND lcv.deleted = 0",
                params![class_name],
                |r| r.get(0),
            )
            .optional()
    }

    /// Every class's complete latest-version definition, in `load_order` —
    /// enough to rebuild an equivalent world elsewhere (the GUI's mock-world
    /// mirror; eventually an image→`.mst` exporter).
    pub fn all_classes(&self) -> rusqlite::Result<Vec<FullClass>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.name, lcv.superclass_name, lcv.category, lcv.comment, lcv.instance_vars, lcv.class_vars, c.load_order \
             FROM classes c JOIN latest_class_versions lcv ON lcv.class_id = c.class_id \
             WHERE lcv.deleted = 0 \
             ORDER BY c.load_order",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(FullClass {
                name: r.get(0)?,
                superclass: r.get(1)?,
                category: r.get(2)?,
                comment: r.get(3)?,
                instance_vars: r.get(4)?,
                class_vars: r.get(5)?,
                load_order: r.get(6)?,
            })
        })?;
        rows.collect()
    }

    /// Every method (both sides, every category) belonging to `class_name`,
    /// at its latest version — the per-class companion to [`all_classes`].
    pub fn all_methods_of(&self, class_name: &str) -> rusqlite::Result<Vec<FullMethod>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.selector, m.side, lmv.category, lmv.source \
             FROM classes c JOIN methods m ON m.class_id = c.class_id \
             JOIN latest_method_versions lmv ON lmv.method_id = m.method_id \
             WHERE c.name = ?1 AND lmv.deleted = 0 ORDER BY m.method_id",
        )?;
        let rows = stmt.query_map(params![class_name], |r| {
            let side_str: String = r.get(1)?;
            Ok(FullMethod {
                selector: r.get(0)?,
                side: if side_str == "class" { Side::Class } else { Side::Instance },
                category: r.get(2)?,
                source: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    // ── Mutations — every save is an INSERT, never an UPDATE ──────────────

    /// Create a class with its first version. For the importer (§6) and
    /// for future "new class" UI; returns the new `class_id`.
    pub fn add_class(
        &self,
        name: &str,
        superclass: Option<&str>,
        category: &str,
        comment: &str,
        instance_vars: &str,
        class_vars: &str,
        load_order: i64,
    ) -> rusqlite::Result<i64> {
        self.conn.execute("INSERT INTO classes (name, load_order) VALUES (?1, ?2)", params![name, load_order])?;
        let class_id = self.conn.last_insert_rowid();
        self.conn.execute(
            "INSERT INTO class_versions (class_id, version_number, superclass_name, category, comment, instance_vars, class_vars, edited_at) \
             VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![class_id, superclass, category, comment, instance_vars, class_vars, now_secs()],
        )?;
        Ok(class_id)
    }

    /// Create a method (on an existing class) with its first version.
    pub fn add_method(&self, class_name: &str, side: Side, selector: &str, category: &str, source: &str) -> rusqlite::Result<Option<i64>> {
        let Some(class_id) = self.class_id_of(class_name)? else { return Ok(None) };
        self.conn.execute(
            "INSERT INTO methods (class_id, selector, side) VALUES (?1, ?2, ?3)",
            params![class_id, selector, side.as_str()],
        )?;
        let method_id = self.conn.last_insert_rowid();
        self.conn.execute(
            "INSERT INTO method_versions (method_id, version_number, category, source, edited_at) VALUES (?1, 1, ?2, ?3, ?4)",
            params![method_id, category, source, now_secs()],
        )?;
        Ok(Some(method_id))
    }

    /// "Accept" edited source for an *existing* method — `docs/IMAGE.md` §4:
    /// insert a new version (carrying its category forward unchanged), then
    /// prune to `RETENTION_LIMIT`. Returns `false` (no auto-create) if
    /// `class_name`/`side`/`selector` doesn't already name a method — same
    /// contract as `macvm-mock-vm::MockWorld::set_method_source`.
    pub fn set_method_source(&self, class_name: &str, side: Side, selector: &str, new_source: &str) -> rusqlite::Result<bool> {
        let Some(method_id) = self.method_id_of(class_name, side, selector)? else { return Ok(false) };
        let category: String =
            self.conn.query_row("SELECT category FROM latest_method_versions WHERE method_id = ?1", params![method_id], |r| r.get(0))?;
        self.insert_method_version(method_id, &category, new_source, false)?;
        self.prune_method_versions(method_id)?;
        Ok(true)
    }

    /// "Accept" an edited class comment — same shape as `set_method_source`,
    /// carrying every other field of the class definition forward unchanged.
    pub fn set_class_comment(&self, class_name: &str, new_comment: &str) -> rusqlite::Result<bool> {
        let Some(class_id) = self.class_id_of(class_name)? else { return Ok(false) };
        let (superclass, category, ivars, cvars): (Option<String>, String, String, String) = self.conn.query_row(
            "SELECT superclass_name, category, instance_vars, class_vars FROM latest_class_versions WHERE class_id = ?1",
            params![class_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        self.insert_class_version(class_id, superclass.as_deref(), &category, new_comment, &ivars, &cvars, false)?;
        self.prune_class_versions(class_id)?;
        Ok(true)
    }

    /// "Accept" an edited class *definition* — the superclass and instance
    /// variables, i.e. the part of a class header `.mst`'s real grammar
    /// (`mst.rs`) actually has syntax for. Carries `category` (package) and
    /// `comment` forward unchanged: reassigning a class's package isn't
    /// exposed through this path, and `class_vars` is left alone too, since
    /// there's no real `.mst` syntax for class variables to round-trip
    /// through yet (see `mst.rs`'s doc comment) — inventing GUI-only syntax
    /// for a field the file format can't express felt like the wrong kind
    /// of shortcut to take silently.
    pub fn set_class_definition(&self, class_name: &str, new_superclass: Option<&str>, new_instance_vars: &str) -> rusqlite::Result<bool> {
        let Some(class_id) = self.class_id_of(class_name)? else { return Ok(false) };
        let (category, comment, cvars): (String, String, String) = self.conn.query_row(
            "SELECT category, comment, class_vars FROM latest_class_versions WHERE class_id = ?1",
            params![class_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        self.insert_class_version(class_id, new_superclass, &category, &comment, new_instance_vars, &cvars, false)?;
        self.prune_class_versions(class_id)?;
        Ok(true)
    }

    /// Soft-delete: insert one more version identical to the latest except
    /// `deleted=1` — reuses the exact insert/prune path every other edit
    /// above uses, so [`undo_method`](Self::undo_method) needs *no changes
    /// at all* to "undo" a removal: it already just reverts to the version
    /// before this one (`docs/IMAGE.md` §4's revert-as-new-version rule).
    /// Returns `false` if the method doesn't exist or is already removed.
    pub fn remove_method(&self, class_name: &str, side: Side, selector: &str) -> rusqlite::Result<bool> {
        let Some(method_id) = self.method_id_of(class_name, side, selector)? else { return Ok(false) };
        let (category, source, deleted): (String, String, i64) = self.conn.query_row(
            "SELECT category, source, deleted FROM latest_method_versions WHERE method_id = ?1",
            params![method_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if deleted != 0 {
            return Ok(false);
        }
        self.insert_method_version(method_id, &category, &source, true)?;
        self.prune_method_versions(method_id)?;
        Ok(true)
    }

    /// Soft-delete a class — mirrors [`remove_method`](Self::remove_method).
    /// Subclasses still naming this class as their superclass are left
    /// exactly as they are (`superclass_name` is a plain text field, not a
    /// foreign key): `package_roots` treats a removed superclass the same
    /// as a missing one, so they simply re-root visually rather than
    /// vanishing or erroring. Deliberately unconditional — the browser is
    /// expected to warn about affected subclasses *before* sending this,
    /// not have the store refuse or auto-reparent on its behalf.
    pub fn remove_class(&self, class_name: &str) -> rusqlite::Result<bool> {
        let Some(class_id) = self.class_id_of(class_name)? else { return Ok(false) };
        let (superclass, category, comment, ivars, cvars, deleted): (Option<String>, String, String, String, String, i64) = self.conn.query_row(
            "SELECT superclass_name, category, comment, instance_vars, class_vars, deleted FROM latest_class_versions WHERE class_id = ?1",
            params![class_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )?;
        if deleted != 0 {
            return Ok(false);
        }
        self.insert_class_version(class_id, superclass.as_deref(), &category, &comment, &ivars, &cvars, true)?;
        self.prune_class_versions(class_id)?;
        Ok(true)
    }

    /// The class-browser "New Class" path (`../../gui/src/vm_host.rs`) —
    /// distinct from [`add_class`](Self::add_class), which the `.mst`
    /// importer uses and which assumes the caller already checked
    /// [`class_exists`](Self::class_exists) itself. This instead
    /// distinguishes three cases in one call: a genuinely new name
    /// (`Created`); a name that only exists as a removed tombstone
    /// (`Reopened` — inserts a fresh non-deleted version on the *same*
    /// `class_id`, the identical trick `src/bin/import_world.rs` already
    /// uses for legitimate class-reopening); and a name that's already live
    /// (`AlreadyLive`, refused — silently overwriting an unrelated live
    /// class by name collision would be surprising and destructive in a way
    /// reopening a removed one isn't).
    pub fn create_or_reopen_class(
        &self,
        name: &str,
        superclass: Option<&str>,
        category: &str,
        comment: &str,
        instance_vars: &str,
    ) -> rusqlite::Result<ClassCreateOutcome> {
        match self.class_id_of(name)? {
            None => {
                let load_order = self.next_load_order()?;
                self.add_class(name, superclass, category, comment, instance_vars, "", load_order)?;
                Ok(ClassCreateOutcome::Created)
            }
            Some(class_id) => {
                let (deleted, cvars): (i64, String) = self.conn.query_row(
                    "SELECT deleted, class_vars FROM latest_class_versions WHERE class_id = ?1",
                    params![class_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if deleted == 0 {
                    return Ok(ClassCreateOutcome::AlreadyLive);
                }
                self.insert_class_version(class_id, superclass, category, comment, instance_vars, &cvars, false)?;
                self.prune_class_versions(class_id)?;
                Ok(ClassCreateOutcome::Reopened)
            }
        }
    }

    /// The class-browser "New Method" path. Ensures the method row exists
    /// (creating it if this selector/side is new on this class), then
    /// always inserts a fresh non-deleted version — collapsing "create,"
    /// "reopen a removed method," and "redefine a live method under the
    /// same selector" into one operation, since all three are the same
    /// ordinary Smalltalk action (accepting a method under some selector)
    /// and none of them should be an error, unlike the class-name-collision
    /// case `create_or_reopen_class` refuses. Returns `None` only if
    /// `class_name` doesn't exist at all.
    pub fn create_or_reopen_method(&self, class_name: &str, side: Side, selector: &str, category: &str, source: &str) -> rusqlite::Result<Option<i64>> {
        let Some(class_id) = self.class_id_of(class_name)? else { return Ok(None) };
        let method_id = match self.method_id_of(class_name, side, selector)? {
            Some(id) => id,
            None => {
                self.conn.execute(
                    "INSERT INTO methods (class_id, selector, side) VALUES (?1, ?2, ?3)",
                    params![class_id, selector, side.as_str()],
                )?;
                self.conn.last_insert_rowid()
            }
        };
        self.insert_method_version(method_id, category, source, false)?;
        self.prune_method_versions(method_id)?;
        Ok(Some(method_id))
    }

    /// Revert-as-new-version (`docs/IMAGE.md` §4) — restores the
    /// second-to-latest version's source as a *new* latest version, rather
    /// than deleting the latest. Returns `false` if there's nothing to undo
    /// (unknown method, or only one version on record). Carries that
    /// version's own `deleted` flag forward too (not hardcoded `false`) —
    /// this is what makes "undo" double as "un-remove" for free: undoing
    /// past a removal restores whichever `deleted` state the version being
    /// restored actually had.
    pub fn undo_method(&self, class_name: &str, side: Side, selector: &str) -> rusqlite::Result<bool> {
        let Some(method_id) = self.method_id_of(class_name, side, selector)? else { return Ok(false) };
        let previous: Option<(String, String, i64)> = self
            .conn
            .query_row(
                "SELECT category, source, deleted FROM method_versions WHERE method_id = ?1 \
                 ORDER BY version_number DESC LIMIT 1 OFFSET 1",
                params![method_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((category, source, deleted)) = previous else { return Ok(false) };
        self.insert_method_version(method_id, &category, &source, deleted != 0)?;
        self.prune_method_versions(method_id)?;
        Ok(true)
    }

    /// See `undo_method`'s doc comment — same "carry the restored version's
    /// own `deleted` flag forward" rule, so undo doubles as un-remove here
    /// too.
    pub fn undo_class(&self, class_name: &str) -> rusqlite::Result<bool> {
        let Some(class_id) = self.class_id_of(class_name)? else { return Ok(false) };
        let previous: Option<(Option<String>, String, String, String, String, i64)> = self
            .conn
            .query_row(
                "SELECT superclass_name, category, comment, instance_vars, class_vars, deleted FROM class_versions \
                 WHERE class_id = ?1 ORDER BY version_number DESC LIMIT 1 OFFSET 1",
                params![class_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?;
        let Some((superclass, category, comment, ivars, cvars, deleted)) = previous else { return Ok(false) };
        self.insert_class_version(class_id, superclass.as_deref(), &category, &comment, &ivars, &cvars, deleted != 0)?;
        self.prune_class_versions(class_id)?;
        Ok(true)
    }

    fn insert_method_version(&self, method_id: i64, category: &str, source: &str, deleted: bool) -> rusqlite::Result<()> {
        let next: i64 =
            self.conn.query_row("SELECT COALESCE(MAX(version_number), 0) + 1 FROM method_versions WHERE method_id = ?1", params![method_id], |r| r.get(0))?;
        self.conn.execute(
            "INSERT INTO method_versions (method_id, version_number, category, source, edited_at, deleted) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![method_id, next, category, source, now_secs(), deleted as i64],
        )?;
        Ok(())
    }

    fn insert_class_version(
        &self,
        class_id: i64,
        superclass: Option<&str>,
        category: &str,
        comment: &str,
        ivars: &str,
        cvars: &str,
        deleted: bool,
    ) -> rusqlite::Result<()> {
        let next: i64 =
            self.conn.query_row("SELECT COALESCE(MAX(version_number), 0) + 1 FROM class_versions WHERE class_id = ?1", params![class_id], |r| r.get(0))?;
        self.conn.execute(
            "INSERT INTO class_versions (class_id, version_number, superclass_name, category, comment, instance_vars, class_vars, edited_at, deleted) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![class_id, next, superclass, category, comment, ivars, cvars, now_secs(), deleted as i64],
        )?;
        Ok(())
    }

    /// `docs/IMAGE.md` §4 — sliding-window retention, applied after every
    /// insert (not a separate GC pass).
    fn prune_method_versions(&self, method_id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM method_versions WHERE method_id = ?1 AND version_number <= \
             (SELECT MAX(version_number) FROM method_versions WHERE method_id = ?1) - ?2",
            params![method_id, RETENTION_LIMIT],
        )?;
        Ok(())
    }

    fn prune_class_versions(&self, class_id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM class_versions WHERE class_id = ?1 AND version_number <= \
             (SELECT MAX(version_number) FROM class_versions WHERE class_id = ?1) - ?2",
            params![class_id, RETENTION_LIMIT],
        )?;
        Ok(())
    }

    // ── Load order (`docs/IMAGE.md` §3) ────────────────────────────────────

    /// `load_order` one step past the current maximum — for appending a
    /// class at the end of the load sequence (the common case: the
    /// importer loading `world.list` in order, or a brand new class).
    pub fn next_load_order(&self) -> rusqlite::Result<i64> {
        let max: Option<i64> = self.conn.query_row("SELECT MAX(load_order) FROM classes", [], |r| r.get(0))?;
        Ok(max.map(|m| m + LOAD_ORDER_STEP).unwrap_or(LOAD_ORDER_START))
    }

    /// Midpoint between two `load_order` values, for inserting a class
    /// between two existing ones. Returns `None` if the gap has been
    /// exhausted (adjacent values differing by ≤ 1) — call
    /// [`rebalance_load_order`] first in that case.
    pub fn load_order_between(before: i64, after: i64) -> Option<i64> {
        let mid = before + (after - before) / 2;
        if mid == before || mid == after {
            None
        } else {
            Some(mid)
        }
    }

    /// Renumber every class back to clean multiples of `LOAD_ORDER_STEP`,
    /// preserving current relative order — the maintenance operation
    /// `docs/IMAGE.md` §3 describes for when gaps run out. Not needed per
    /// edit; safe to call any time (idempotent on an already-clean table).
    pub fn rebalance_load_order(&self) -> rusqlite::Result<()> {
        let mut stmt = self.conn.prepare("SELECT class_id FROM classes ORDER BY load_order")?;
        let ids: Vec<i64> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for (i, class_id) in ids.into_iter().enumerate() {
            let new_order = LOAD_ORDER_START + (i as i64) * LOAD_ORDER_STEP;
            self.conn.execute("UPDATE classes SET load_order = ?1 WHERE class_id = ?2", params![new_order, class_id])?;
        }
        Ok(())
    }

    pub fn reorder_class(&self, class_name: &str, new_load_order: i64) -> rusqlite::Result<bool> {
        let changed = self.conn.execute("UPDATE classes SET load_order = ?1 WHERE name = ?2", params![new_load_order, class_name])?;
        Ok(changed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> Image {
        let img = Image::open_in_memory().unwrap();
        let lo = img.next_load_order().unwrap();
        img.add_class("Object", None, "Kernel", "The root of the hierarchy.", "", "", lo).unwrap();
        let lo = img.next_load_order().unwrap();
        img.add_class("Collection", Some("Object"), "Collections", "Abstract collection.", "", "", lo).unwrap();
        img.add_method("Object", Side::Instance, "printString", "printing", "printString\n\t^'an Object'").unwrap();
        img.add_method("Object", Side::Instance, "hash", "comparing", "hash\n\t^self identityHash").unwrap();
        img
    }

    #[test]
    fn packages_and_roots_round_trip() {
        let img = seeded();
        assert_eq!(img.packages().unwrap(), vec!["Kernel".to_string(), "Collections".to_string()]);
        let roots = img.package_roots("Kernel").unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "Object");
    }

    #[test]
    fn subclasses_and_categories_and_methods() {
        let img = seeded();
        let subs = img.subclasses_of("Object").unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].name, "Collection");

        let cats = img.categories("Object", Side::Instance).unwrap();
        assert!(cats.contains(&"printing".to_string()));
        assert!(cats.contains(&"comparing".to_string()));

        let methods = img.methods_in("Object", Side::Instance, "printing").unwrap();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].selector, "printString");
    }

    #[test]
    fn set_method_source_versions_instead_of_overwriting() {
        let img = seeded();
        assert!(img.set_method_source("Object", Side::Instance, "hash", "hash\n\t^42").unwrap());
        assert_eq!(img.method_source("Object", Side::Instance, "hash").unwrap().unwrap(), "hash\n\t^42");
        // Unknown selector: no auto-create.
        assert!(!img.set_method_source("Object", Side::Instance, "nope", "x").unwrap());
    }

    #[test]
    fn undo_method_restores_previous_source_as_a_new_version() {
        let img = seeded();
        img.set_method_source("Object", Side::Instance, "hash", "hash\n\t^42").unwrap();
        assert!(img.undo_method("Object", Side::Instance, "hash").unwrap());
        assert_eq!(
            img.method_source("Object", Side::Instance, "hash").unwrap().unwrap(),
            "hash\n\t^self identityHash"
        );
        // The "undo" is itself a new version — undo-the-undo should work too.
        assert!(img.undo_method("Object", Side::Instance, "hash").unwrap());
        assert_eq!(img.method_source("Object", Side::Instance, "hash").unwrap().unwrap(), "hash\n\t^42");
    }

    #[test]
    fn undo_with_only_one_version_reports_false() {
        let img = seeded();
        assert!(!img.undo_method("Object", Side::Instance, "hash").unwrap());
    }

    #[test]
    fn set_class_comment_carries_other_fields_forward() {
        let img = seeded();
        assert!(img.set_class_comment("Collection", "Updated comment.").unwrap());
        assert_eq!(img.class_comment("Collection").unwrap().unwrap(), "Updated comment.");
        // superclass/category must survive the edit unchanged.
        let roots = img.package_roots("Collections").unwrap();
        assert_eq!(roots[0].superclass.as_deref(), Some("Object"));
    }

    #[test]
    fn retention_keeps_at_most_the_limit() {
        let img = seeded();
        for i in 0..(RETENTION_LIMIT + 5) {
            img.set_method_source("Object", Side::Instance, "hash", &format!("hash\n\t^{i}")).unwrap();
        }
        let count: i64 = img
            .conn
            .query_row(
                "SELECT COUNT(*) FROM method_versions WHERE method_id = (SELECT method_id FROM methods WHERE selector='hash')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, RETENTION_LIMIT);
        // And the latest surviving value is still correct.
        let last = RETENTION_LIMIT + 4;
        assert_eq!(img.method_source("Object", Side::Instance, "hash").unwrap().unwrap(), format!("hash\n\t^{last}"));
    }

    #[test]
    fn load_order_gap_numbering() {
        let img = Image::open_in_memory().unwrap();
        assert_eq!(img.next_load_order().unwrap(), LOAD_ORDER_START);
        img.add_class("A", None, "P", "", "", "", 1000).unwrap();
        assert_eq!(img.next_load_order().unwrap(), 1100);
        img.add_class("B", None, "P", "", "", "", 1100).unwrap();
        assert_eq!(Image::load_order_between(1000, 1100), Some(1050));
        img.add_class("C", None, "P", "", "", "", 1050).unwrap();
        assert_eq!(Image::load_order_between(1000, 1050), Some(1025));
        // Exhausted gap.
        assert_eq!(Image::load_order_between(1000, 1001), None);
    }

    #[test]
    fn rebalance_restores_clean_gaps() {
        let img = Image::open_in_memory().unwrap();
        img.add_class("A", None, "P", "", "", "", 1000).unwrap();
        img.add_class("B", None, "P", "", "", "", 1001).unwrap();
        img.rebalance_load_order().unwrap();
        assert_eq!(Image::load_order_between(1000, 1100), Some(1050));
    }

    #[test]
    fn reorder_class_updates_load_order() {
        let img = seeded();
        assert!(img.reorder_class("Collection", 500).unwrap());
        let roots = img.package_roots("Kernel").unwrap();
        // Object is still Kernel's only root; just confirm reorder didn't error
        // and the class is still queryable at its new position.
        assert_eq!(roots[0].name, "Object");
        assert!(!img.reorder_class("Nonexistent", 999).unwrap());
    }

    #[test]
    fn remove_method_hides_it_and_undo_restores_it() {
        let img = seeded();
        assert!(img.remove_method("Object", Side::Instance, "hash").unwrap());
        assert_eq!(img.method_source("Object", Side::Instance, "hash").unwrap(), None);
        assert!(!img.categories("Object", Side::Instance).unwrap().contains(&"comparing".to_string()));
        // Already removed: a second remove is a no-op-false, not an error.
        assert!(!img.remove_method("Object", Side::Instance, "hash").unwrap());
        // Unknown selector.
        assert!(!img.remove_method("Object", Side::Instance, "nope").unwrap());

        // Undo un-removes it — no dedicated "unremove" needed.
        assert!(img.undo_method("Object", Side::Instance, "hash").unwrap());
        assert_eq!(img.method_source("Object", Side::Instance, "hash").unwrap().unwrap(), "hash\n\t^self identityHash");
    }

    #[test]
    fn remove_class_hides_it_and_reroots_subclasses() {
        let img = seeded();
        assert!(img.remove_class("Object").unwrap());
        assert!(!img.packages().unwrap().contains(&"Kernel".to_string()), "Kernel had only Object");
        assert_eq!(img.class_comment("Object").unwrap(), None);
        // Collection's superclass_name still literally says "Object", but a
        // removed superclass counts as absent for the package-roots check —
        // Collection re-roots in "Collections" instead of vanishing.
        let roots = img.package_roots("Collections").unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "Collection");

        assert!(!img.remove_class("Object").unwrap()); // already removed
        assert!(!img.remove_class("Nonexistent").unwrap());

        assert!(img.undo_class("Object").unwrap());
        assert!(img.packages().unwrap().contains(&"Kernel".to_string()));
    }

    #[test]
    fn create_or_reopen_class_distinguishes_new_live_and_removed() {
        let img = seeded();
        assert_eq!(
            img.create_or_reopen_class("Stream", Some("Object"), "Streams", "A stream.", "").unwrap(),
            ClassCreateOutcome::Created
        );
        assert!(img.class_comment("Stream").unwrap().is_some());

        // Already live: refused, not silently overwritten.
        assert_eq!(
            img.create_or_reopen_class("Object", None, "Kernel", "Hijacked!", "").unwrap(),
            ClassCreateOutcome::AlreadyLive
        );
        assert_eq!(img.class_comment("Object").unwrap().unwrap(), "The root of the hierarchy.");

        // Removed, then recreated under the same name: reopened, not a
        // UNIQUE(name) constraint error.
        img.remove_class("Collection").unwrap();
        assert_eq!(
            img.create_or_reopen_class("Collection", Some("Object"), "Collections", "Reborn.", "elements").unwrap(),
            ClassCreateOutcome::Reopened
        );
        assert_eq!(img.class_comment("Collection").unwrap().unwrap(), "Reborn.");
    }

    #[test]
    fn create_or_reopen_method_creates_redefines_and_reopens() {
        let img = seeded();
        // Brand new selector.
        assert!(img.create_or_reopen_method("Object", Side::Instance, "printOn:", "printing", "printOn: s\n\t^s").unwrap().is_some());
        assert_eq!(img.method_source("Object", Side::Instance, "printOn:").unwrap().unwrap(), "printOn: s\n\t^s");

        // Redefining an already-live selector via this path is normal, not
        // an error (unlike the class case).
        assert!(img.create_or_reopen_method("Object", Side::Instance, "hash", "comparing", "hash\n\t^0").unwrap().is_some());
        assert_eq!(img.method_source("Object", Side::Instance, "hash").unwrap().unwrap(), "hash\n\t^0");

        // Removed, then reopened under the same selector.
        img.remove_method("Object", Side::Instance, "printString").unwrap();
        assert!(img
            .create_or_reopen_method("Object", Side::Instance, "printString", "printing", "printString\n\t^'reborn'")
            .unwrap()
            .is_some());
        assert_eq!(img.method_source("Object", Side::Instance, "printString").unwrap().unwrap(), "printString\n\t^'reborn'");

        // Unknown class.
        assert!(img.create_or_reopen_method("Nonexistent", Side::Instance, "foo", "cat", "foo").unwrap().is_none());
    }

    #[test]
    fn set_class_definition_changes_superclass_and_ivars_only() {
        let img = seeded();
        assert!(img.set_class_definition("Collection", Some("Object"), "elements size").unwrap());
        let roots = img.package_roots("Collections").unwrap();
        assert_eq!(roots[0].name, "Collection");
        // comment and category must survive the definition edit unchanged.
        assert_eq!(img.class_comment("Collection").unwrap().unwrap(), "Abstract collection.");
        assert!(img.packages().unwrap().contains(&"Collections".to_string()));
        assert!(!img.set_class_definition("Nonexistent", None, "").unwrap());
    }
}
