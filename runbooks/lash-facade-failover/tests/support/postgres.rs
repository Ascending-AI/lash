//! A PostgreSQL server the case owns, so it can stop and start it.
//!
//! The server is a pinned PostgreSQL tree (`LASH_WORKERS_POSTGRES`, a
//! directory with `bin/initdb`, `bin/postgres` and `bin/pg_ctl`; the kiln
//! target hands it `native//:postgres`). `initdb` looks its user up in the
//! password database, and a pool action runs as a user its image does not
//! list: `LASH_WORKERS_NSS_WRAPPER`, the pinned `libnss_wrapper.so`, answers
//! that lookup when it is set. The cluster lives in a temporary directory
//! and dies with the case.
//!
//! [`Server::start`] provisions two databases: `lash`, from the store's
//! published schema, and `lash_witness`, from `witness.sql`.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use sqlx::Executor as _;
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::process::{Child, Command};

const USER: &str = "lash";
const READY: Duration = Duration::from_secs(60);
const WITNESS_SQL: &str = include_str!("../../witness.sql");

/// The server settings: loopback only, room for every node's pools, and
/// nothing made durable that a fast shutdown does not flush anyway.
const SETTINGS: &[(&str, &str)] = &[
    ("listen_addresses", "127.0.0.1"),
    ("unix_socket_directories", ""),
    ("max_connections", "200"),
    ("max_locks_per_transaction", "256"),
    ("fsync", "off"),
    ("synchronous_commit", "off"),
    ("full_page_writes", "off"),
    ("timezone", "UTC"),
    ("log_timezone", "UTC"),
];

/// One running (or stopped) server.
pub struct Server {
    tree: PathBuf,
    nss_wrapper: Option<PathBuf>,
    work: tempfile::TempDir,
    port: u16,
    child: Option<Child>,
}

fn tree() -> PathBuf {
    match std::env::var_os("LASH_WORKERS_POSTGRES") {
        Some(tree) => PathBuf::from(tree),
        None => panic!(
            "LASH_WORKERS_POSTGRES is not set: point it at a PostgreSQL 18 tree with bin/initdb \
             (the kiln target sets it to native//:postgres)"
        ),
    }
}

fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free loopback port");
    probe.local_addr().expect("a bound address").port()
}

impl Server {
    /// Initialize a cluster, start it, and provision `lash` and
    /// `lash_witness`.
    pub async fn start() -> Self {
        let tree = std::fs::canonicalize(tree()).expect("the PostgreSQL tree exists");
        let nss_wrapper = std::env::var_os("LASH_WORKERS_NSS_WRAPPER")
            .map(|path| std::fs::canonicalize(path).expect("the NSS wrapper exists"));
        let work = tempfile::Builder::new()
            .prefix("lash-facade-failover-")
            .tempdir()
            .expect("a temporary directory");
        let mut server = Self {
            tree,
            nss_wrapper,
            work,
            port: free_port(),
            child: None,
        };
        server.initdb().await;
        server.launch().await;
        server.provision().await;
        server
    }

    fn data(&self) -> PathBuf {
        self.work.path().join("data")
    }

    fn bin(&self, name: &str) -> PathBuf {
        self.tree.join("bin").join(name)
    }

    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        if let Some(wrapper) = &self.nss_wrapper {
            let passwd = self.work.path().join("passwd");
            let group = self.work.path().join("group");
            command
                .env("LD_PRELOAD", wrapper)
                .env("NSS_WRAPPER_PASSWD", passwd)
                .env("NSS_WRAPPER_GROUP", group);
        }
        command
    }

    async fn initdb(&self) {
        if self.nss_wrapper.is_some() {
            // The directory this process just made is owned by its own ids.
            let owner = std::fs::metadata(self.work.path()).expect("the work directory");
            let (uid, gid) = (owner.uid(), owner.gid());
            let home = self.work.path().display().to_string();
            std::fs::write(
                self.work.path().join("passwd"),
                format!("{USER}:x:{uid}:{gid}::{home}:/bin/false\n"),
            )
            .expect("write passwd");
            std::fs::write(self.work.path().join("group"), format!("{USER}:x:{gid}:\n"))
                .expect("write group");
        }
        let output = self
            .command(&self.bin("initdb"))
            .arg("--pgdata")
            .arg(self.data())
            .args(["--username", USER, "--auth", "trust", "--encoding", "UTF8"])
            .args(["--locale", "C", "--no-sync"])
            .output()
            .await
            .expect("initdb runs");
        assert!(
            output.status.success(),
            "initdb failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn launch(&mut self) {
        let log = std::fs::File::options()
            .create(true)
            .append(true)
            .open(self.work.path().join("postgres.log"))
            .expect("open the server log");
        let mut command = self.command(&self.bin("postgres"));
        command
            .arg("-D")
            .arg(self.data())
            .arg("-c")
            .arg(format!("port={}", self.port));
        for (name, value) in SETTINGS {
            command.arg("-c").arg(format!("{name}={value}"));
        }
        let child = command
            .stdout(log.try_clone().expect("clone the log handle"))
            .stderr(log)
            .spawn()
            .expect("postgres starts");
        self.child = Some(child);
        let deadline = Instant::now() + READY;
        loop {
            if let Ok(pool) = self.pool("postgres").await
                && pool.execute("SELECT 1").await.is_ok()
            {
                pool.close().await;
                return;
            }
            assert!(
                Instant::now() < deadline,
                "postgres was not ready after {READY:?}:\n{}",
                self.log_tail()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn provision(&self) {
        let admin = self.pool("postgres").await.expect("connect as the owner");
        for database in ["lash", "lash_witness"] {
            admin
                .execute(format!("CREATE DATABASE {database}").as_str())
                .await
                .expect("create a database");
        }
        admin.close().await;
        let lash = self.pool("lash").await.expect("connect to lash");
        sqlx::raw_sql(lash_postgres_store::PostgresStorage::schema_ddl())
            .execute(&lash)
            .await
            .expect("apply the lash schema");
        lash.close().await;
        let witness = self
            .pool("lash_witness")
            .await
            .expect("connect to the witness");
        sqlx::raw_sql(WITNESS_SQL)
            .execute(&witness)
            .await
            .expect("apply witness.sql");
        witness.close().await;
    }

    /// A pool on `database` as the cluster's owner.
    pub async fn pool(&self, database: &str) -> Result<PgPool, sqlx::Error> {
        PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(2))
            .connect(&self.url(database, USER))
            .await
    }

    /// The URL of `database` as `user`.
    pub fn url(&self, database: &str, user: &str) -> String {
        format!("postgres://{user}@127.0.0.1:{}/{database}", self.port)
    }

    /// Stop the server with a fast shutdown: sessions end now, and every
    /// acknowledged commit is flushed.
    pub async fn stop(&mut self) {
        let status = self
            .command(&self.bin("pg_ctl"))
            .arg("-D")
            .arg(self.data())
            .args(["-m", "fast", "-w", "-t", "60", "stop"])
            .stdout(Stdio::null())
            .status()
            .await
            .expect("pg_ctl runs");
        assert!(status.success(), "pg_ctl stop failed:\n{}", self.log_tail());
        if let Some(mut child) = self.child.take() {
            child.wait().await.expect("postgres exits");
        }
    }

    /// Start the stopped server again, on the same port and data.
    pub async fn restart(&mut self) {
        self.launch().await;
    }

    /// The end of the server's log.
    pub fn log_tail(&self) -> String {
        let log =
            std::fs::read_to_string(self.work.path().join("postgres.log")).unwrap_or_default();
        let start = log.len().saturating_sub(4096);
        log.get(start..).unwrap_or(&log).to_owned()
    }
}
