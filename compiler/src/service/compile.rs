use std::{
    collections::HashSet,
    num::NonZeroUsize,
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
    thread::JoinHandle,
};

use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use typst::{
    doc::{Frame, FrameItem, Position},
    geom::Point,
    syntax::{LinkedNode, Source, Span, SyntaxKind, VirtualPath},
    World,
};

use crate::{
    vfs::notify::{FilesystemEvent, MemoryEvent, NotifyMessage},
    world::{CompilerFeat, CompilerWorld},
    ShadowApi,
};
use typst_ts_core::{
    error::prelude::ZResult, vector::span_id_from_u64, TypstDocument, TypstFileId,
};

use super::{Compiler, DiagObserver, WorkspaceProvider, WorldExporter};

/// A task that can be sent to the context (compiler thread)
///
/// The internal function will be dereferenced and called on the context.
type BorrowTask<Ctx> = Box<dyn FnOnce(&mut Ctx) + Send + 'static>;

/// Interrupts for the compiler thread.
enum CompilerInterrupt<Ctx> {
    /// Interrupted by task.
    ///
    /// See [`CompileClient<Ctx>::steal`] for more information.
    Task(BorrowTask<Ctx>),
    /// Interrupted by memory file changes.
    Memory(MemoryEvent),
    /// Interrupted by file system event.
    ///
    /// If the event is `None`, it means the initial file system scan is done.
    /// Otherwise, it means a file system event is received.
    Fs(Option<FilesystemEvent>),
}

/// Responses from the compiler thread.
enum CompilerResponse {
    /// Response to the file watcher
    Notify(NotifyMessage),
}

/// A tagged memory event with logical tick.
struct TaggedMemoryEvent {
    /// The logical tick when the event is received.
    logical_tick: usize,
    /// The memory event happened.
    event: MemoryEvent,
}

/// The compiler thread.
pub struct CompileActor<C: Compiler> {
    /// The underlying compiler.
    pub compiler: C,
    /// The root path of the workspace.
    pub root: PathBuf,
    /// Whether to enable file system watching.
    pub enable_watch: bool,

    /// The current logical tick.
    logical_tick: usize,
    /// Last logical tick when invalidation is caused by shadow update.
    dirty_shadow_logical_tick: usize,

    /// Estimated latest set of shadow files.
    estimated_shadow_files: HashSet<Arc<Path>>,
    /// The latest compiled document.
    latest_doc: Option<Arc<TypstDocument>>,

    /// Internal channel for stealing the compiler thread.
    steal_send: mpsc::UnboundedSender<BorrowTask<Self>>,
    steal_recv: mpsc::UnboundedReceiver<BorrowTask<Self>>,

    /// Internal channel for memory events.
    memory_send: mpsc::UnboundedSender<MemoryEvent>,
    memory_recv: mpsc::UnboundedReceiver<MemoryEvent>,
}

impl<C: Compiler + ShadowApi + WorldExporter + Send + 'static> CompileActor<C>
where
    C::World: for<'files> codespan_reporting::files::Files<'files, FileId = TypstFileId>,
{
    /// Create a new compiler thread.
    pub fn new(compiler: C, root: PathBuf) -> Self {
        let (steal_send, steal_recv) = mpsc::unbounded_channel();
        let (memory_send, memory_recv) = mpsc::unbounded_channel();

        Self {
            compiler,
            root,

            logical_tick: 1,
            enable_watch: false,
            dirty_shadow_logical_tick: 0,

            estimated_shadow_files: Default::default(),
            latest_doc: None,

            steal_send,
            steal_recv,

            memory_send,
            memory_recv,
        }
    }

    /// Run the compiler thread synchronously.
    pub fn run(self) -> bool {
        use tokio::runtime::Handle;

        if Handle::try_current().is_err() && self.enable_watch {
            log::error!("Typst compiler thread with watch enabled must be run in a tokio runtime");
            return false;
        }

        tokio::task::block_in_place(move || Handle::current().block_on(self.block_run_inner()))
    }

    /// Inner function for `run`, it launches the compiler thread and blocks
    /// until it exits.
    async fn block_run_inner(mut self) -> bool {
        if !self.enable_watch {
            let compiled = self
                .compiler
                .with_stage_diag::<false, _>("compiling", |driver| driver.compile());
            return compiled.is_some();
        }

        if let Some(h) = self.spawn().await {
            // Note: this is blocking the current thread.
            // Note: the block safety is ensured by `run` function.
            h.join().unwrap();
        }

        true
    }

    /// Spawn the compiler thread.
    pub async fn spawn(mut self) -> Option<JoinHandle<()>> {
        if !self.enable_watch {
            self.compiler
                .with_stage_diag::<false, _>("compiling", |driver| driver.compile());
            return None;
        }

        // Setup internal channels.
        let (dep_tx, dep_rx) = tokio::sync::mpsc::unbounded_channel();
        let (fs_tx, mut fs_rx) = tokio::sync::mpsc::unbounded_channel();

        // Wrap sender to send compiler response.
        let compiler_ack = move |res: CompilerResponse| match res {
            CompilerResponse::Notify(msg) => dep_tx.send(msg).unwrap(),
        };

        // Spawn file system watcher.
        tokio::spawn(super::watch_deps(dep_rx, move |event| {
            fs_tx.send(event).unwrap();
        }));

        // Spawn compiler thread.
        let compile_thread = ensure_single_thread("typst-compiler", async move {
            log::debug!("CompileActor: initialized");

            // Wait for first events.
            while let Some(event) = tokio::select! {
                Some(it) = fs_rx.recv() => Some(CompilerInterrupt::Fs(it)),
                Some(it) = self.memory_recv.recv() => Some(CompilerInterrupt::Memory(it)),
                Some(it) = self.steal_recv.recv() => Some(CompilerInterrupt::Task(it)),
            } {
                // Small step to warp the logical clock.
                self.logical_tick += 1;

                // Accumulate events.
                let mut need_recompile = false;
                need_recompile = self.process(event, &compiler_ack) || need_recompile;
                while let Some(event) = fs_rx
                    .try_recv()
                    .ok()
                    .map(CompilerInterrupt::Fs)
                    .or_else(|| {
                        self.memory_recv
                            .try_recv()
                            .ok()
                            .map(CompilerInterrupt::Memory)
                    })
                    .or_else(|| self.steal_recv.try_recv().ok().map(CompilerInterrupt::Task))
                {
                    need_recompile = self.process(event, &compiler_ack) || need_recompile;
                }

                // Compile if needed.
                if need_recompile {
                    self.compile(&compiler_ack);
                }
            }

            log::debug!("CompileActor: exited");
        })
        .unwrap();

        // Return the thread handle.
        Some(compile_thread)
    }

    /// Compile the document.
    fn compile(&mut self, send: impl Fn(CompilerResponse)) {
        use CompilerResponse::*;

        // Compile the document.
        self.latest_doc = self
            .compiler
            .with_stage_diag::<true, _>("compiling", |driver| driver.compile());

        // Evict compilation cache.
        comemo::evict(30);

        // Notify the new file dependencies.
        let mut deps = vec![];
        self.compiler
            .iter_dependencies(&mut |dep, _| deps.push(dep.clone()));
        send(Notify(NotifyMessage::SyncDependency(deps)));
    }

    /// Process some interrupt.
    fn process(&mut self, event: CompilerInterrupt<Self>, send: impl Fn(CompilerResponse)) -> bool {
        use CompilerResponse::*;
        // warp the logical clock by one.
        self.logical_tick += 1;

        match event {
            // Borrow the compiler thread and run the task.
            //
            // See [`CompileClient::steal`] for more information.
            CompilerInterrupt::Task(task) => {
                log::debug!("CompileActor: execute task");

                task(self);

                // Will never trigger compilation
                false
            }
            // Handle memory events.
            CompilerInterrupt::Memory(event) => {
                log::debug!("CompileActor: memory event incoming");

                // Emulate memory changes.
                let mut files = HashSet::new();
                if matches!(event, MemoryEvent::Sync(..)) {
                    files = self.estimated_shadow_files.clone();
                    self.estimated_shadow_files.clear();
                }
                match &event {
                    MemoryEvent::Sync(event) | MemoryEvent::Update(event) => {
                        for path in event.removes.iter().map(Deref::deref) {
                            self.estimated_shadow_files.remove(path);
                            files.insert(path.into());
                        }
                        for path in event.inserts.iter().map(|e| e.0.deref()) {
                            self.estimated_shadow_files.insert(path.into());
                            files.remove(path);
                        }
                    }
                }

                // If there is no invalidation happening, apply memory changes directly.
                if files.is_empty() && self.dirty_shadow_logical_tick == 0 {
                    self.apply_memory_changes(event);

                    // Will trigger compilation
                    return true;
                }

                // Otherwise, send upstream update event.
                // Also, record the logical tick when shadow is dirty.
                self.dirty_shadow_logical_tick = self.logical_tick;
                send(Notify(NotifyMessage::UpstreamUpdate(
                    crate::vfs::notify::UpstreamUpdateEvent {
                        invalidates: files.into_iter().collect(),
                        opaque: Box::new(TaggedMemoryEvent {
                            logical_tick: self.logical_tick,
                            event,
                        }),
                    },
                )));

                // Delayed trigger compilation
                false
            }
            // Handle file system events.
            CompilerInterrupt::Fs(event) => {
                log::debug!("CompileActor: fs event incoming {:?}", event);

                // Handle file system event if any.
                if let Some(mut event) = event {
                    // Handle delayed upstream update event before applying file system changes
                    if let FilesystemEvent::UpstreamUpdate { upstream_event, .. } = &mut event {
                        let event = upstream_event.take().unwrap().opaque;
                        let TaggedMemoryEvent {
                            logical_tick,
                            event,
                        } = *event.downcast().unwrap();

                        // Recovery from dirty shadow state.
                        if logical_tick == self.dirty_shadow_logical_tick {
                            self.dirty_shadow_logical_tick = 0;
                        }

                        self.apply_memory_changes(event);
                    }

                    // Apply file system changes.
                    self.compiler.notify_fs_event(event);
                }

                // Will trigger compilation
                true
            }
        }
    }

    /// Apply memory changes to underlying compiler.
    fn apply_memory_changes(&mut self, event: MemoryEvent) {
        if matches!(event, MemoryEvent::Sync(..)) {
            self.compiler.reset_shadow();
        }
        match event {
            MemoryEvent::Update(event) | MemoryEvent::Sync(event) => {
                for removes in event.removes {
                    let _ = self.compiler.unmap_shadow(&removes);
                }
                for (p, t) in event.inserts {
                    let _ = self.compiler.map_shadow(&p, t.content().cloned().unwrap());
                }
            }
        }
    }
}

impl<C: Compiler> CompileActor<C> {
    pub fn with_watch(mut self, enable_watch: bool) -> Self {
        self.enable_watch = enable_watch;
        self
    }

    pub fn split(self) -> (Self, CompileClient<Self>) {
        let steal_send = self.steal_send.clone();
        let memory_send = self.memory_send.clone();
        (
            self,
            CompileClient {
                steal_send,
                memory_send,
                _ctx: std::marker::PhantomData,
            },
        )
    }

    pub fn document(&self) -> Option<Arc<TypstDocument>> {
        self.latest_doc.clone()
    }
}
pub struct CompileClient<Ctx> {
    steal_send: mpsc::UnboundedSender<BorrowTask<Ctx>>,
    memory_send: mpsc::UnboundedSender<MemoryEvent>,

    _ctx: std::marker::PhantomData<Ctx>,
}

impl<Ctx> CompileClient<Ctx> {
    fn steal_inner<Ret: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut Ctx) -> Ret + Send + 'static,
    ) -> oneshot::Receiver<Ret> {
        let (tx, rx) = oneshot::channel();

        self.steal_send
            .send(Box::new(move |this: &mut Ctx| {
                if tx.send(f(this)).is_err() {
                    // Receiver was dropped. The main thread may have exited, or the request may
                    // have been cancelled.
                    log::warn!("could not send back return value from Typst thread");
                }
            }))
            .unwrap();
        rx
    }

    pub fn steal<Ret: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut Ctx) -> Ret + Send + 'static,
    ) -> ZResult<Ret> {
        Ok(self.steal_inner(f).blocking_recv().unwrap())
    }

    /// Steal the compiler thread and run the given function.
    pub async fn steal_async<Ret: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut Ctx, tokio::runtime::Handle) -> Ret + Send + 'static,
    ) -> ZResult<Ret> {
        // get current async handle
        let handle = tokio::runtime::Handle::current();
        Ok(self
            .steal_inner(move |this: &mut Ctx| f(this, handle.clone()))
            .await
            .unwrap())
    }

    pub fn add_memory_changes(&self, event: MemoryEvent) {
        self.memory_send.send(event).unwrap();
    }
}

#[derive(Debug, Serialize)]
pub struct DocToSrcJumpInfo {
    filepath: String,
    start: Option<(usize, usize)>, // row, column
    end: Option<(usize, usize)>,
}

// todo: remove constraint to CompilerWorld
impl<F: CompilerFeat, Ctx: Compiler<World = CompilerWorld<F>>> CompileClient<CompileActor<Ctx>>
where
    Ctx::World: WorkspaceProvider,
{
    /// fixme: character is 0-based, UTF-16 code unit.
    /// We treat it as UTF-8 now.
    pub async fn resolve_src_to_doc_jump(
        &mut self,
        filepath: PathBuf,
        line: usize,
        character: usize,
    ) -> ZResult<Option<Position>> {
        self.steal_async(move |this, _| {
            let doc = this.document()?;

            let world = this.compiler.world();

            let relative_path = filepath
                .strip_prefix(&this.compiler.world().workspace_root())
                .ok()?;

            let source_id = TypstFileId::new(None, VirtualPath::new(relative_path));
            let source = world.source(source_id).ok()?;
            let cursor = source.line_column_to_byte(line, character)?;

            jump_from_cursor(&doc.pages, &source, cursor)
        })
        .await
    }

    pub async fn resolve_doc_to_src_jump(&mut self, id: u64) -> ZResult<Option<DocToSrcJumpInfo>> {
        let resolve_off =
            |src: &Source, off: usize| src.byte_to_line(off).zip(src.byte_to_column(off));

        self.steal_async(move |this, _| {
            let world = this.compiler.world();
            let span = span_id_from_u64(id)?;
            let src_id = span.id()?;
            let source = world.source(src_id).ok()?;
            let range = source.find(span)?.range();
            let filepath = world.path_for_id(src_id).ok()?;
            Some(DocToSrcJumpInfo {
                filepath: filepath.to_string_lossy().to_string(),
                start: resolve_off(&source, range.start),
                end: resolve_off(&source, range.end),
            })
        })
        .await
    }
}

/// Spawn a thread and run the given future on it.
///
/// Note: the future is run on a single-threaded tokio runtime.
fn ensure_single_thread<F: std::future::Future<Output = ()> + Send + 'static>(
    name: &str,
    f: F,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name(name.to_owned()).spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f);
    })
}

/// Find the output location in the document for a cursor position.
pub fn jump_from_cursor(frames: &[Frame], source: &Source, cursor: usize) -> Option<Position> {
    let node = LinkedNode::new(source.root()).leaf_at(cursor)?;
    if node.kind() != SyntaxKind::Text {
        return None;
    }

    let mut min_dis = u64::MAX;
    let mut p = Point::default();
    let mut ppage = 0usize;

    let span = node.span();
    for (i, frame) in frames.iter().enumerate() {
        let t_dis = min_dis;
        if let Some(pos) = find_in_frame(frame, span, &mut min_dis, &mut p) {
            return Some(Position {
                page: NonZeroUsize::new(i + 1).unwrap(),
                point: pos,
            });
        }
        if t_dis != min_dis {
            ppage = i;
        }
    }

    if min_dis == u64::MAX {
        return None;
    }

    Some(Position {
        page: NonZeroUsize::new(ppage + 1).unwrap(),
        point: p,
    })
}

/// Find the position of a span in a frame.
fn find_in_frame(frame: &Frame, span: Span, min_dis: &mut u64, p: &mut Point) -> Option<Point> {
    for (mut pos, item) in frame.items() {
        if let FrameItem::Group(group) = item {
            // TODO: Handle transformation.
            if let Some(point) = find_in_frame(&group.frame, span, min_dis, p) {
                return Some(point + pos);
            }
        }

        if let FrameItem::Text(text) = item {
            for glyph in &text.glyphs {
                if glyph.span.0 == span {
                    return Some(pos);
                }
                if glyph.span.0.id() == span.id() {
                    let dis = glyph.span.0.number().abs_diff(span.number());
                    if dis < *min_dis {
                        *min_dis = dis;
                        *p = pos;
                    }
                }
                pos.x += glyph.x_advance.at(text.size);
            }
        }
    }

    None
}