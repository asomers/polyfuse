//! test script:
//!
//! ```shell-session
//! $ for i in {1..1000}; do clear; cat /path/to/heartbeat; usleep 500000; done
//! ```

#![allow(clippy::unnecessary_mut_passed)]
#![deny(clippy::unimplemented)]

use polyfuse::{
    notify::{InvalInode, Retrieve, Store},
    reply::{AttrOut, FileAttr, OpenOut, Reply},
    Config, MountOptions, Operation, Request, Session,
};
use polyfuse_example_async_std_support::{AsyncConnection, Writer};

use anyhow::{anyhow, ensure, Context as _, Result};
use async_std::sync::Mutex;
use chrono::Local;
use futures::{channel::oneshot, prelude::*};
use std::{
    collections::HashMap,
    io, mem,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

const ROOT_INO: u64 = 1;

#[async_std::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = pico_args::Arguments::from_env();

    let no_notify = args.contains("--no-notify");
    let notify_kind = args
        .opt_value_from_os_str("--notify-kind", |s| match s.to_str() {
            Some("store") => Ok(NotifyKind::Store),
            Some("invalidate") => Ok(NotifyKind::Invalidate),
            s => Err(anyhow!("invalid notify kind: {:?}", s)),
        })?
        .unwrap_or(NotifyKind::Store);
    let update_interval: u64 = args.value_from_str("--update-interval")?;

    let mountpoint: PathBuf = args.free_from_str()?.context("missing mountpoint")?;
    ensure!(mountpoint.is_file(), "mountpoint must be a regular file");

    let conn = AsyncConnection::open(mountpoint, MountOptions::default()).await?;
    let session = Session::start(&conn, &conn, Config::default()).await?;

    let heartbeat = Arc::new(Heartbeat::now());

    // Spawn a task that beats the heart.
    let _: async_std::task::JoinHandle<io::Result<()>> = async_std::task::spawn({
        let heartbeat = heartbeat.clone();
        let writer = if !no_notify {
            Some(conn.writer())
        } else {
            None
        };
        async move {
            loop {
                tracing::info!("heartbeat");

                heartbeat.update_content().await;

                if let Some(ref writer) = writer {
                    match notify_kind {
                        NotifyKind::Store => heartbeat.notify_store(&writer).await?,
                        NotifyKind::Invalidate => heartbeat.notify_inval_inode(&writer).await?,
                    }
                }

                async_std::task::sleep(Duration::from_secs(update_interval)).await;
            }
        }
    });

    // Run the filesystem daemon on the foreground.
    while let Some(req) = session.next_request(&conn).await? {
        let heartbeat = heartbeat.clone();
        let writer = conn.writer();

        let _: async_std::task::JoinHandle<Result<()>> = async_std::task::spawn(async move {
            let span = tracing::debug_span!("handle request", unique = req.unique());

            let op = req.operation()?;
            span.in_scope(|| tracing::debug!(?op));

            let reply = ReplyWriter { req: &req, writer };

            match op {
                Operation::Getattr(op) => match op.ino() {
                    ROOT_INO => {
                        let inner = heartbeat.inner.lock().await;
                        let mut out = AttrOut::default();
                        fill_attr(out.attr(), &inner.attr);
                        reply.ok(out)?;
                    }
                    _ => reply.error(libc::ENOENT)?,
                },
                Operation::Open(op) => match op.ino() {
                    ROOT_INO => {
                        let mut out = OpenOut::default();
                        out.keep_cache(true);
                        reply.ok(out)?;
                    }
                    _ => reply.error(libc::ENOENT)?,
                },
                Operation::Read(op) => match op.ino() {
                    ROOT_INO => {
                        let inner = heartbeat.inner.lock().await;

                        let offset = op.offset() as usize;
                        if offset >= inner.content.len() {
                            reply.ok(&[])?;
                        } else {
                            let size = op.size() as usize;
                            let data = &inner.content.as_bytes()[offset..];
                            let data = &data[..std::cmp::min(data.len(), size)];
                            reply.ok(data)?;
                        }
                    }
                    _ => reply.error(libc::ENOENT)?,
                },
                Operation::NotifyReply(op, mut data) => {
                    if let Some(tx) = heartbeat.retrieves.lock().await.remove(&op.unique()) {
                        let mut buf = vec![0u8; op.size() as usize];
                        data.read_exact(&mut buf).await?;
                        let _ = tx.send(buf);
                    }
                }

                _ => reply.error(libc::ENOSYS)?,
            }

            Ok(())
        });
    }

    Ok(())
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum NotifyKind {
    Store,
    Invalidate,
}

struct Heartbeat {
    inner: Mutex<Inner>,
    retrieves: Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>,
    retrieve_unique: AtomicU64,
}

struct Inner {
    content: String,
    attr: libc::stat,
}

impl Heartbeat {
    fn now() -> Self {
        let content = Local::now().to_rfc3339();

        let mut attr = unsafe { mem::zeroed::<libc::stat>() };
        attr.st_ino = ROOT_INO;
        attr.st_mode = libc::S_IFREG | 0o444;
        attr.st_size = content.len() as libc::off_t;

        Self {
            inner: Mutex::new(Inner { content, attr }),
            retrieves: Mutex::default(),
            retrieve_unique: AtomicU64::default(),
        }
    }

    async fn update_content(&self) {
        let mut inner = self.inner.lock().await;
        let content = Local::now().to_rfc3339();
        inner.attr.st_size = content.len() as libc::off_t;
        inner.content = content;
    }

    async fn notify_store(&self, writer: &Writer) -> io::Result<()> {
        let inner = self.inner.lock().await;
        let content = &inner.content;

        tracing::info!("send notify_store(data={:?})", content);
        polyfuse::bytes::write_bytes(writer, Store::new(ROOT_INO, 0, content))?;

        // To check if the cache is updated correctly, pull the
        // content from the kernel using notify_retrieve.
        tracing::info!("send notify_retrieve");
        let data = {
            // FIXME: choose appropriate atomic ordering.
            let unique = self.retrieve_unique.fetch_add(1, Ordering::SeqCst);
            polyfuse::bytes::write_bytes(writer, Retrieve::new(unique, ROOT_INO, 0, 1024))?;
            let (tx, rx) = oneshot::channel();
            self.retrieves.lock().await.insert(unique, tx);
            rx.await.unwrap()
        };
        tracing::info!("--> content={:?}", data);

        if data[..content.len()] != *content.as_bytes() {
            tracing::error!("mismatched data");
        }

        Ok(())
    }

    async fn notify_inval_inode(&self, writer: &Writer) -> io::Result<()> {
        tracing::info!("send notify_invalidate_inode");
        polyfuse::bytes::write_bytes(writer, InvalInode::new(ROOT_INO, 0, 0))?;
        Ok(())
    }
}

fn fill_attr(attr: &mut FileAttr, st: &libc::stat) {
    attr.ino(st.st_ino);
    attr.size(st.st_size as u64);
    attr.mode(st.st_mode);
    attr.nlink(st.st_nlink as u32);
    attr.uid(st.st_uid);
    attr.gid(st.st_gid);
    attr.rdev(st.st_rdev as u32);
    attr.blksize(st.st_blksize as u32);
    attr.blocks(st.st_blocks as u64);
    attr.atime(Duration::new(st.st_atime as u64, st.st_atime_nsec as u32));
    attr.mtime(Duration::new(st.st_mtime as u64, st.st_mtime_nsec as u32));
    attr.ctime(Duration::new(st.st_ctime as u64, st.st_ctime_nsec as u32));
}

struct ReplyWriter<'req> {
    req: &'req Request,
    writer: Writer,
}

impl ReplyWriter<'_> {
    fn ok<T>(self, arg: T) -> io::Result<()>
    where
        T: polyfuse::bytes::Bytes,
    {
        polyfuse::bytes::write_bytes(&self.writer, Reply::new(self.req.unique(), 0, arg))
    }

    fn error(self, code: i32) -> io::Result<()> {
        polyfuse::bytes::write_bytes(&self.writer, Reply::new(self.req.unique(), code, ()))
    }
}
