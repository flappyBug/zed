mod anchor;

pub use anchor::{Anchor, AnchorRangeExt};
use anyhow::Result;
use clock::ReplicaId;
use collections::{HashMap, HashSet};
use gpui::{AppContext, Entity, ModelContext, ModelHandle, Task};
pub use language::Completion;
use language::{
    Buffer, BufferChunks, BufferSnapshot, Chunk, DiagnosticEntry, Event, File, Language, Outline,
    OutlineItem, Selection, ToOffset as _, ToPoint as _, TransactionId,
};
use std::{
    cell::{Ref, RefCell},
    cmp, fmt, io,
    iter::{self, FromIterator},
    ops::{Range, Sub},
    str,
    sync::Arc,
    time::{Duration, Instant},
};
use sum_tree::{Bias, Cursor, SumTree};
use text::{
    locator::Locator,
    rope::TextDimension,
    subscription::{Subscription, Topic},
    AnchorRangeExt as _, Edit, Point, PointUtf16, TextSummary,
};
use theme::SyntaxTheme;
use util::post_inc;

const NEWLINES: &'static [u8] = &[b'\n'; u8::MAX as usize];

pub type ExcerptId = Locator;

pub struct MultiBuffer {
    snapshot: RefCell<MultiBufferSnapshot>,
    buffers: RefCell<HashMap<usize, BufferState>>,
    subscriptions: Topic,
    singleton: bool,
    replica_id: ReplicaId,
    history: History,
}

struct History {
    next_transaction_id: usize,
    undo_stack: Vec<Transaction>,
    redo_stack: Vec<Transaction>,
    transaction_depth: usize,
    group_interval: Duration,
}

#[derive(Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Debug)]
pub enum CharKind {
    Newline,
    Punctuation,
    Whitespace,
    Word,
}

struct Transaction {
    id: usize,
    buffer_transactions: HashSet<(usize, text::TransactionId)>,
    first_edit_at: Instant,
    last_edit_at: Instant,
}

pub trait ToOffset: 'static + fmt::Debug {
    fn to_offset(&self, snapshot: &MultiBufferSnapshot) -> usize;
}

pub trait ToPoint: 'static + fmt::Debug {
    fn to_point(&self, snapshot: &MultiBufferSnapshot) -> Point;
}

struct BufferState {
    buffer: ModelHandle<Buffer>,
    last_version: clock::Global,
    last_parse_count: usize,
    last_selections_update_count: usize,
    last_diagnostics_update_count: usize,
    excerpts: Vec<ExcerptId>,
    _subscriptions: [gpui::Subscription; 2],
}

#[derive(Clone, Default)]
pub struct MultiBufferSnapshot {
    singleton: bool,
    excerpts: SumTree<Excerpt>,
    parse_count: usize,
    diagnostics_update_count: usize,
    is_dirty: bool,
    has_conflict: bool,
}

pub struct ExcerptProperties<'a, T> {
    pub buffer: &'a ModelHandle<Buffer>,
    pub range: Range<T>,
}

#[derive(Clone)]
struct Excerpt {
    id: ExcerptId,
    buffer_id: usize,
    buffer: BufferSnapshot,
    range: Range<text::Anchor>,
    max_buffer_row: u32,
    text_summary: TextSummary,
    has_trailing_newline: bool,
}

#[derive(Clone, Debug, Default)]
struct ExcerptSummary {
    excerpt_id: ExcerptId,
    max_buffer_row: u32,
    text: TextSummary,
}

pub struct MultiBufferRows<'a> {
    buffer_row_range: Range<u32>,
    excerpts: Cursor<'a, Excerpt, Point>,
}

pub struct MultiBufferChunks<'a> {
    range: Range<usize>,
    excerpts: Cursor<'a, Excerpt, usize>,
    excerpt_chunks: Option<ExcerptChunks<'a>>,
    theme: Option<&'a SyntaxTheme>,
}

pub struct MultiBufferBytes<'a> {
    range: Range<usize>,
    excerpts: Cursor<'a, Excerpt, usize>,
    excerpt_bytes: Option<ExcerptBytes<'a>>,
    chunk: &'a [u8],
}

struct ExcerptChunks<'a> {
    content_chunks: BufferChunks<'a>,
    footer_height: usize,
}

struct ExcerptBytes<'a> {
    content_bytes: language::rope::Bytes<'a>,
    footer_height: usize,
}

impl MultiBuffer {
    pub fn new(replica_id: ReplicaId) -> Self {
        Self {
            snapshot: Default::default(),
            buffers: Default::default(),
            subscriptions: Default::default(),
            singleton: false,
            replica_id,
            history: History {
                next_transaction_id: Default::default(),
                undo_stack: Default::default(),
                redo_stack: Default::default(),
                transaction_depth: 0,
                group_interval: Duration::from_millis(300),
            },
        }
    }

    pub fn singleton(buffer: ModelHandle<Buffer>, cx: &mut ModelContext<Self>) -> Self {
        let mut this = Self::new(buffer.read(cx).replica_id());
        this.singleton = true;
        this.push_excerpt(
            ExcerptProperties {
                buffer: &buffer,
                range: text::Anchor::min()..text::Anchor::max(),
            },
            cx,
        );
        this.snapshot.borrow_mut().singleton = true;
        this
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn build_simple(text: &str, cx: &mut gpui::MutableAppContext) -> ModelHandle<Self> {
        let buffer = cx.add_model(|cx| Buffer::new(0, text, cx));
        cx.add_model(|cx| Self::singleton(buffer, cx))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn build_random(
        mut rng: &mut impl rand::Rng,
        cx: &mut gpui::MutableAppContext,
    ) -> ModelHandle<Self> {
        use rand::prelude::*;
        use std::env;
        use text::RandomCharIter;

        let max_excerpts = env::var("MAX_EXCERPTS")
            .map(|i| i.parse().expect("invalid `MAX_EXCERPTS` variable"))
            .unwrap_or(5);
        let excerpts = rng.gen_range(1..=max_excerpts);

        cx.add_model(|cx| {
            let mut multibuffer = MultiBuffer::new(0);
            let mut buffers = Vec::new();
            for _ in 0..excerpts {
                let buffer_handle = if rng.gen() || buffers.is_empty() {
                    let text = RandomCharIter::new(&mut rng).take(10).collect::<String>();
                    buffers.push(cx.add_model(|cx| Buffer::new(0, text, cx)));
                    let buffer = buffers.last().unwrap();
                    log::info!(
                        "Creating new buffer {} with text: {:?}",
                        buffer.id(),
                        buffer.read(cx).text()
                    );
                    buffers.last().unwrap()
                } else {
                    buffers.choose(rng).unwrap()
                };

                let buffer = buffer_handle.read(cx);
                let end_ix = buffer.clip_offset(rng.gen_range(0..=buffer.len()), Bias::Right);
                let start_ix = buffer.clip_offset(rng.gen_range(0..=end_ix), Bias::Left);
                let header_height = rng.gen_range(0..=5);
                log::info!(
                    "Inserting excerpt from buffer {} with header height {} and range {:?}: {:?}",
                    buffer_handle.id(),
                    header_height,
                    start_ix..end_ix,
                    &buffer.text()[start_ix..end_ix]
                );

                multibuffer.push_excerpt(
                    ExcerptProperties {
                        buffer: buffer_handle,
                        range: start_ix..end_ix,
                    },
                    cx,
                );
            }
            multibuffer
        })
    }

    pub fn replica_id(&self) -> ReplicaId {
        self.replica_id
    }

    pub fn snapshot(&self, cx: &AppContext) -> MultiBufferSnapshot {
        self.sync(cx);
        self.snapshot.borrow().clone()
    }

    pub fn read(&self, cx: &AppContext) -> Ref<MultiBufferSnapshot> {
        self.sync(cx);
        self.snapshot.borrow()
    }

    pub fn as_singleton(&self) -> Option<ModelHandle<Buffer>> {
        if self.singleton {
            return Some(
                self.buffers
                    .borrow()
                    .values()
                    .next()
                    .unwrap()
                    .buffer
                    .clone(),
            );
        } else {
            None
        }
    }

    pub fn subscribe(&mut self) -> Subscription {
        self.subscriptions.subscribe()
    }

    pub fn edit<I, S, T>(&mut self, ranges: I, new_text: T, cx: &mut ModelContext<Self>)
    where
        I: IntoIterator<Item = Range<S>>,
        S: ToOffset,
        T: Into<String>,
    {
        self.edit_internal(ranges, new_text, false, cx)
    }

    pub fn edit_with_autoindent<I, S, T>(
        &mut self,
        ranges: I,
        new_text: T,
        cx: &mut ModelContext<Self>,
    ) where
        I: IntoIterator<Item = Range<S>>,
        S: ToOffset,
        T: Into<String>,
    {
        self.edit_internal(ranges, new_text, true, cx)
    }

    pub fn edit_internal<I, S, T>(
        &mut self,
        ranges_iter: I,
        new_text: T,
        autoindent: bool,
        cx: &mut ModelContext<Self>,
    ) where
        I: IntoIterator<Item = Range<S>>,
        S: ToOffset,
        T: Into<String>,
    {
        if let Some(buffer) = self.as_singleton() {
            let snapshot = self.read(cx);
            let ranges = ranges_iter
                .into_iter()
                .map(|range| range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot));
            return buffer.update(cx, |buffer, cx| {
                if autoindent {
                    buffer.edit_with_autoindent(ranges, new_text, cx)
                } else {
                    buffer.edit(ranges, new_text, cx)
                }
            });
        }

        let snapshot = self.read(cx);
        let mut buffer_edits: HashMap<usize, Vec<(Range<usize>, bool)>> = Default::default();
        let mut cursor = snapshot.excerpts.cursor::<usize>();
        for range in ranges_iter {
            let start = range.start.to_offset(&snapshot);
            let end = range.end.to_offset(&snapshot);
            cursor.seek(&start, Bias::Right, &());
            if cursor.item().is_none() && start == *cursor.start() {
                cursor.prev(&());
            }
            let start_excerpt = cursor.item().expect("start offset out of bounds");
            let start_overshoot = start - cursor.start();
            let buffer_start =
                start_excerpt.range.start.to_offset(&start_excerpt.buffer) + start_overshoot;

            cursor.seek(&end, Bias::Right, &());
            if cursor.item().is_none() && end == *cursor.start() {
                cursor.prev(&());
            }
            let end_excerpt = cursor.item().expect("end offset out of bounds");
            let end_overshoot = end - cursor.start();
            let buffer_end = end_excerpt.range.start.to_offset(&end_excerpt.buffer) + end_overshoot;

            if start_excerpt.id == end_excerpt.id {
                buffer_edits
                    .entry(start_excerpt.buffer_id)
                    .or_insert(Vec::new())
                    .push((buffer_start..buffer_end, true));
            } else {
                let start_excerpt_range =
                    buffer_start..start_excerpt.range.end.to_offset(&start_excerpt.buffer);
                let end_excerpt_range =
                    end_excerpt.range.start.to_offset(&end_excerpt.buffer)..buffer_end;
                buffer_edits
                    .entry(start_excerpt.buffer_id)
                    .or_insert(Vec::new())
                    .push((start_excerpt_range, true));
                buffer_edits
                    .entry(end_excerpt.buffer_id)
                    .or_insert(Vec::new())
                    .push((end_excerpt_range, false));

                cursor.seek(&start, Bias::Right, &());
                cursor.next(&());
                while let Some(excerpt) = cursor.item() {
                    if excerpt.id == end_excerpt.id {
                        break;
                    }
                    buffer_edits
                        .entry(excerpt.buffer_id)
                        .or_insert(Vec::new())
                        .push((excerpt.range.to_offset(&excerpt.buffer), false));
                    cursor.next(&());
                }
            }
        }

        let new_text = new_text.into();
        for (buffer_id, mut edits) in buffer_edits {
            edits.sort_unstable_by_key(|(range, _)| range.start);
            self.buffers.borrow()[&buffer_id]
                .buffer
                .update(cx, |buffer, cx| {
                    let mut edits = edits.into_iter().peekable();
                    let mut insertions = Vec::new();
                    let mut deletions = Vec::new();
                    while let Some((mut range, mut is_insertion)) = edits.next() {
                        while let Some((next_range, next_is_insertion)) = edits.peek() {
                            if range.end >= next_range.start {
                                range.end = cmp::max(next_range.end, range.end);
                                is_insertion |= *next_is_insertion;
                                edits.next();
                            } else {
                                break;
                            }
                        }

                        if is_insertion {
                            insertions.push(
                                buffer.anchor_before(range.start)..buffer.anchor_before(range.end),
                            );
                        } else if !range.is_empty() {
                            deletions.push(
                                buffer.anchor_before(range.start)..buffer.anchor_before(range.end),
                            );
                        }
                    }

                    if autoindent {
                        buffer.edit_with_autoindent(deletions, "", cx);
                        buffer.edit_with_autoindent(insertions, new_text.clone(), cx);
                    } else {
                        buffer.edit(deletions, "", cx);
                        buffer.edit(insertions, new_text.clone(), cx);
                    }
                })
        }
    }

    pub fn start_transaction(&mut self, cx: &mut ModelContext<Self>) -> Option<TransactionId> {
        self.start_transaction_at(Instant::now(), cx)
    }

    pub(crate) fn start_transaction_at(
        &mut self,
        now: Instant,
        cx: &mut ModelContext<Self>,
    ) -> Option<TransactionId> {
        if let Some(buffer) = self.as_singleton() {
            return buffer.update(cx, |buffer, _| buffer.start_transaction_at(now));
        }

        for BufferState { buffer, .. } in self.buffers.borrow().values() {
            buffer.update(cx, |buffer, _| buffer.start_transaction_at(now));
        }
        self.history.start_transaction(now)
    }

    pub fn end_transaction(&mut self, cx: &mut ModelContext<Self>) -> Option<TransactionId> {
        self.end_transaction_at(Instant::now(), cx)
    }

    pub(crate) fn end_transaction_at(
        &mut self,
        now: Instant,
        cx: &mut ModelContext<Self>,
    ) -> Option<TransactionId> {
        if let Some(buffer) = self.as_singleton() {
            return buffer.update(cx, |buffer, cx| buffer.end_transaction_at(now, cx));
        }

        let mut buffer_transactions = HashSet::default();
        for BufferState { buffer, .. } in self.buffers.borrow().values() {
            if let Some(transaction_id) =
                buffer.update(cx, |buffer, cx| buffer.end_transaction_at(now, cx))
            {
                buffer_transactions.insert((buffer.id(), transaction_id));
            }
        }

        if self.history.end_transaction(now, buffer_transactions) {
            let transaction_id = self.history.group().unwrap();
            Some(transaction_id)
        } else {
            None
        }
    }

    pub fn avoid_grouping_next_transaction(&mut self, cx: &mut ModelContext<Self>) {
        for BufferState { buffer, .. } in self.buffers.borrow().values() {
            buffer.update(cx, |buffer, _| buffer.avoid_grouping_next_transaction());
        }
    }

    pub fn set_active_selections(
        &mut self,
        selections: &[Selection<Anchor>],
        cx: &mut ModelContext<Self>,
    ) {
        let mut selections_by_buffer: HashMap<usize, Vec<Selection<text::Anchor>>> =
            Default::default();
        let snapshot = self.read(cx);
        let mut cursor = snapshot.excerpts.cursor::<Option<&ExcerptId>>();
        for selection in selections {
            cursor.seek(&Some(&selection.start.excerpt_id), Bias::Left, &());
            while let Some(excerpt) = cursor.item() {
                if excerpt.id > selection.end.excerpt_id {
                    break;
                }

                let mut start = excerpt.range.start.clone();
                let mut end = excerpt.range.end.clone();
                if excerpt.id == selection.start.excerpt_id {
                    start = selection.start.text_anchor.clone();
                }
                if excerpt.id == selection.end.excerpt_id {
                    end = selection.end.text_anchor.clone();
                }
                selections_by_buffer
                    .entry(excerpt.buffer_id)
                    .or_default()
                    .push(Selection {
                        id: selection.id,
                        start,
                        end,
                        reversed: selection.reversed,
                        goal: selection.goal,
                    });

                cursor.next(&());
            }
        }

        for (buffer_id, buffer_state) in self.buffers.borrow().iter() {
            if !selections_by_buffer.contains_key(buffer_id) {
                buffer_state
                    .buffer
                    .update(cx, |buffer, cx| buffer.remove_active_selections(cx));
            }
        }

        for (buffer_id, mut selections) in selections_by_buffer {
            self.buffers.borrow()[&buffer_id]
                .buffer
                .update(cx, |buffer, cx| {
                    selections.sort_unstable_by(|a, b| a.start.cmp(&b.start, buffer).unwrap());
                    let mut selections = selections.into_iter().peekable();
                    let merged_selections = Arc::from_iter(iter::from_fn(|| {
                        let mut selection = selections.next()?;
                        while let Some(next_selection) = selections.peek() {
                            if selection
                                .end
                                .cmp(&next_selection.start, buffer)
                                .unwrap()
                                .is_ge()
                            {
                                let next_selection = selections.next().unwrap();
                                if next_selection
                                    .end
                                    .cmp(&selection.end, buffer)
                                    .unwrap()
                                    .is_ge()
                                {
                                    selection.end = next_selection.end;
                                }
                            } else {
                                break;
                            }
                        }
                        Some(selection)
                    }));
                    buffer.set_active_selections(merged_selections, cx);
                });
        }
    }

    pub fn remove_active_selections(&mut self, cx: &mut ModelContext<Self>) {
        for buffer in self.buffers.borrow().values() {
            buffer
                .buffer
                .update(cx, |buffer, cx| buffer.remove_active_selections(cx));
        }
    }

    pub fn undo(&mut self, cx: &mut ModelContext<Self>) -> Option<TransactionId> {
        if let Some(buffer) = self.as_singleton() {
            return buffer.update(cx, |buffer, cx| buffer.undo(cx));
        }

        while let Some(transaction) = self.history.pop_undo() {
            let mut undone = false;
            for (buffer_id, buffer_transaction_id) in &transaction.buffer_transactions {
                if let Some(BufferState { buffer, .. }) = self.buffers.borrow().get(&buffer_id) {
                    undone |= buffer.update(cx, |buf, cx| {
                        buf.undo_transaction(*buffer_transaction_id, cx)
                    });
                }
            }

            if undone {
                return Some(transaction.id);
            }
        }

        None
    }

    pub fn redo(&mut self, cx: &mut ModelContext<Self>) -> Option<TransactionId> {
        if let Some(buffer) = self.as_singleton() {
            return buffer.update(cx, |buffer, cx| buffer.redo(cx));
        }

        while let Some(transaction) = self.history.pop_redo() {
            let mut redone = false;
            for (buffer_id, buffer_transaction_id) in &transaction.buffer_transactions {
                if let Some(BufferState { buffer, .. }) = self.buffers.borrow().get(&buffer_id) {
                    redone |= buffer.update(cx, |buf, cx| {
                        buf.redo_transaction(*buffer_transaction_id, cx)
                    });
                }
            }

            if redone {
                return Some(transaction.id);
            }
        }

        None
    }

    pub fn push_excerpt<O>(
        &mut self,
        props: ExcerptProperties<O>,
        cx: &mut ModelContext<Self>,
    ) -> ExcerptId
    where
        O: text::ToOffset,
    {
        self.insert_excerpt_after(&ExcerptId::max(), props, cx)
    }

    pub fn insert_excerpt_after<O>(
        &mut self,
        prev_excerpt_id: &ExcerptId,
        props: ExcerptProperties<O>,
        cx: &mut ModelContext<Self>,
    ) -> ExcerptId
    where
        O: text::ToOffset,
    {
        assert_eq!(self.history.transaction_depth, 0);
        self.sync(cx);

        let buffer_snapshot = props.buffer.read(cx).snapshot();
        let range = buffer_snapshot.anchor_before(&props.range.start)
            ..buffer_snapshot.anchor_after(&props.range.end);
        let mut snapshot = self.snapshot.borrow_mut();
        let mut cursor = snapshot.excerpts.cursor::<Option<&ExcerptId>>();
        let mut new_excerpts = cursor.slice(&Some(prev_excerpt_id), Bias::Right, &());

        let mut prev_id = ExcerptId::min();
        let edit_start = new_excerpts.summary().text.bytes;
        new_excerpts.update_last(
            |excerpt| {
                excerpt.has_trailing_newline = true;
                prev_id = excerpt.id.clone();
            },
            &(),
        );

        let mut next_id = ExcerptId::max();
        if let Some(next_excerpt) = cursor.item() {
            next_id = next_excerpt.id.clone();
        }

        let id = ExcerptId::between(&prev_id, &next_id);

        let mut buffers = self.buffers.borrow_mut();
        let buffer_state = buffers
            .entry(props.buffer.id())
            .or_insert_with(|| BufferState {
                last_version: buffer_snapshot.version().clone(),
                last_parse_count: buffer_snapshot.parse_count(),
                last_selections_update_count: buffer_snapshot.selections_update_count(),
                last_diagnostics_update_count: buffer_snapshot.diagnostics_update_count(),
                excerpts: Default::default(),
                _subscriptions: [
                    cx.observe(&props.buffer, |_, _, cx| cx.notify()),
                    cx.subscribe(&props.buffer, Self::on_buffer_event),
                ],
                buffer: props.buffer.clone(),
            });
        if let Err(ix) = buffer_state.excerpts.binary_search(&id) {
            buffer_state.excerpts.insert(ix, id.clone());
        }

        let excerpt = Excerpt::new(
            id.clone(),
            props.buffer.id(),
            buffer_snapshot,
            range,
            cursor.item().is_some(),
        );
        new_excerpts.push(excerpt, &());
        let edit_end = new_excerpts.summary().text.bytes;

        new_excerpts.push_tree(cursor.suffix(&()), &());
        drop(cursor);
        snapshot.excerpts = new_excerpts;

        self.subscriptions.publish_mut([Edit {
            old: edit_start..edit_start,
            new: edit_start..edit_end,
        }]);

        cx.notify();
        id
    }

    pub fn excerpt_ids_for_buffer(&self, buffer: &ModelHandle<Buffer>) -> Vec<ExcerptId> {
        self.buffers
            .borrow()
            .get(&buffer.id())
            .map_or(Vec::new(), |state| state.excerpts.clone())
    }

    pub fn excerpted_buffers<'a, T: ToOffset>(
        &'a self,
        range: Range<T>,
        cx: &AppContext,
    ) -> Vec<(ModelHandle<Buffer>, Range<usize>)> {
        let snapshot = self.snapshot(cx);
        let start = range.start.to_offset(&snapshot);
        let end = range.end.to_offset(&snapshot);

        let mut result = Vec::new();
        let mut cursor = snapshot.excerpts.cursor::<usize>();
        cursor.seek(&start, Bias::Right, &());
        while let Some(excerpt) = cursor.item() {
            if *cursor.start() > end {
                break;
            }

            let mut end_before_newline = cursor.end(&());
            if excerpt.has_trailing_newline {
                end_before_newline -= 1;
            }
            let excerpt_start = excerpt.range.start.to_offset(&excerpt.buffer);
            let start = excerpt_start + (cmp::max(start, *cursor.start()) - *cursor.start());
            let end = excerpt_start + (cmp::min(end, end_before_newline) - *cursor.start());
            let buffer = self.buffers.borrow()[&excerpt.buffer_id].buffer.clone();
            result.push((buffer, start..end));
            cursor.next(&());
        }

        result
    }

    pub fn remove_excerpts<'a>(
        &mut self,
        excerpt_ids: impl IntoIterator<Item = &'a ExcerptId>,
        cx: &mut ModelContext<Self>,
    ) {
        let mut buffers = self.buffers.borrow_mut();
        let mut snapshot = self.snapshot.borrow_mut();
        let mut new_excerpts = SumTree::new();
        let mut cursor = snapshot.excerpts.cursor::<(Option<&ExcerptId>, usize)>();
        let mut edits = Vec::new();
        let mut excerpt_ids = excerpt_ids.into_iter().peekable();

        while let Some(mut excerpt_id) = excerpt_ids.next() {
            // Seek to the next excerpt to remove, preserving any preceding excerpts.
            new_excerpts.push_tree(cursor.slice(&Some(excerpt_id), Bias::Left, &()), &());
            if let Some(mut excerpt) = cursor.item() {
                if excerpt.id != *excerpt_id {
                    continue;
                }
                let mut old_start = cursor.start().1;

                // Skip over the removed excerpt.
                loop {
                    if let Some(buffer_state) = buffers.get_mut(&excerpt.buffer_id) {
                        buffer_state.excerpts.retain(|id| id != excerpt_id);
                        if buffer_state.excerpts.is_empty() {
                            buffers.remove(&excerpt.buffer_id);
                        }
                    }
                    cursor.next(&());

                    // Skip over any subsequent excerpts that are also removed.
                    if let Some(&next_excerpt_id) = excerpt_ids.peek() {
                        if let Some(next_excerpt) = cursor.item() {
                            if next_excerpt.id == *next_excerpt_id {
                                excerpt = next_excerpt;
                                excerpt_id = excerpt_ids.next().unwrap();
                                continue;
                            }
                        }
                    }

                    break;
                }

                // When removing the last excerpt, remove the trailing newline from
                // the previous excerpt.
                if cursor.item().is_none() && old_start > 0 {
                    old_start -= 1;
                    new_excerpts.update_last(|e| e.has_trailing_newline = false, &());
                }

                // Push an edit for the removal of this run of excerpts.
                let old_end = cursor.start().1;
                let new_start = new_excerpts.summary().text.bytes;
                edits.push(Edit {
                    old: old_start..old_end,
                    new: new_start..new_start,
                });
            }
        }
        new_excerpts.push_tree(cursor.suffix(&()), &());
        drop(cursor);
        snapshot.excerpts = new_excerpts;
        self.subscriptions.publish_mut(edits);
        cx.notify();
    }

    pub fn text_anchor_for_position<'a, T: ToOffset>(
        &'a self,
        position: T,
        cx: &AppContext,
    ) -> (ModelHandle<Buffer>, language::Anchor) {
        let snapshot = self.read(cx);
        let anchor = snapshot.anchor_before(position);
        (
            self.buffers.borrow()[&anchor.buffer_id].buffer.clone(),
            anchor.text_anchor,
        )
    }

    fn on_buffer_event(
        &mut self,
        _: ModelHandle<Buffer>,
        event: &Event,
        cx: &mut ModelContext<Self>,
    ) {
        cx.emit(event.clone());
    }

    pub fn format(&mut self, cx: &mut ModelContext<Self>) -> Task<Result<()>> {
        let mut format_tasks = Vec::new();
        for BufferState { buffer, .. } in self.buffers.borrow().values() {
            format_tasks.push(buffer.update(cx, |buffer, cx| buffer.format(cx)));
        }

        cx.spawn(|_, _| async move {
            for format in format_tasks {
                format.await?;
            }
            Ok(())
        })
    }

    pub fn save(&mut self, cx: &mut ModelContext<Self>) -> Task<Result<()>> {
        let mut save_tasks = Vec::new();
        for BufferState { buffer, .. } in self.buffers.borrow().values() {
            save_tasks.push(buffer.update(cx, |buffer, cx| buffer.save(cx)));
        }

        cx.spawn(|_, _| async move {
            for save in save_tasks {
                save.await?;
            }
            Ok(())
        })
    }

    pub fn completions<T>(
        &self,
        position: T,
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<Vec<Completion<Anchor>>>>
    where
        T: ToOffset,
    {
        let anchor = self.read(cx).anchor_before(position);
        let buffer = self.buffers.borrow()[&anchor.buffer_id].buffer.clone();
        let completions =
            buffer.update(cx, |buffer, cx| buffer.completions(anchor.text_anchor, cx));
        cx.spawn(|this, cx| async move {
            completions.await.map(|completions| {
                let snapshot = this.read_with(&cx, |buffer, cx| buffer.snapshot(cx));
                completions
                    .into_iter()
                    .map(|completion| Completion {
                        old_range: snapshot.anchor_in_excerpt(
                            anchor.excerpt_id.clone(),
                            completion.old_range.start,
                        )
                            ..snapshot.anchor_in_excerpt(
                                anchor.excerpt_id.clone(),
                                completion.old_range.end,
                            ),
                        new_text: completion.new_text,
                        lsp_completion: completion.lsp_completion,
                    })
                    .collect()
            })
        })
    }

    pub fn is_completion_trigger<T>(&self, position: T, text: &str, cx: &AppContext) -> bool
    where
        T: ToOffset,
    {
        let mut chars = text.chars();
        let char = if let Some(char) = chars.next() {
            char
        } else {
            return false;
        };
        if chars.next().is_some() {
            return false;
        }

        if char.is_alphanumeric() || char == '_' {
            return true;
        }

        let snapshot = self.snapshot(cx);
        let anchor = snapshot.anchor_before(position);
        let buffer = self.buffers.borrow()[&anchor.buffer_id].buffer.clone();
        if let Some(language_server) = buffer.read(cx).language_server() {
            language_server
                .capabilities()
                .completion_provider
                .as_ref()
                .map_or(false, |provider| {
                    provider
                        .trigger_characters
                        .as_ref()
                        .map_or(false, |characters| {
                            characters.iter().any(|string| string == text)
                        })
                })
        } else {
            false
        }
    }

    pub fn apply_additional_edits_for_completion(
        &self,
        completion: Completion<Anchor>,
        cx: &mut ModelContext<Self>,
    ) -> Option<Task<Result<()>>> {
        let buffer = self
            .buffers
            .borrow()
            .get(&completion.old_range.start.buffer_id)?
            .buffer
            .clone();
        buffer.update(cx, |buffer, cx| {
            buffer.apply_additional_edits_for_completion(
                Completion {
                    old_range: completion.old_range.start.text_anchor
                        ..completion.old_range.end.text_anchor,
                    new_text: completion.new_text,
                    lsp_completion: completion.lsp_completion,
                },
                cx,
            )
        })
    }

    pub fn language<'a>(&self, cx: &'a AppContext) -> Option<&'a Arc<Language>> {
        self.buffers
            .borrow()
            .values()
            .next()
            .and_then(|state| state.buffer.read(cx).language())
    }

    pub fn file<'a>(&self, cx: &'a AppContext) -> Option<&'a dyn File> {
        self.as_singleton()?.read(cx).file()
    }

    #[cfg(test)]
    pub fn is_parsing(&self, cx: &AppContext) -> bool {
        self.as_singleton().unwrap().read(cx).is_parsing()
    }

    fn sync(&self, cx: &AppContext) {
        let mut snapshot = self.snapshot.borrow_mut();
        let mut excerpts_to_edit = Vec::new();
        let mut reparsed = false;
        let mut diagnostics_updated = false;
        let mut is_dirty = false;
        let mut has_conflict = false;
        let mut buffers = self.buffers.borrow_mut();
        for buffer_state in buffers.values_mut() {
            let buffer = buffer_state.buffer.read(cx);
            let version = buffer.version();
            let parse_count = buffer.parse_count();
            let selections_update_count = buffer.selections_update_count();
            let diagnostics_update_count = buffer.diagnostics_update_count();

            let buffer_edited = version.changed_since(&buffer_state.last_version);
            let buffer_reparsed = parse_count > buffer_state.last_parse_count;
            let buffer_selections_updated =
                selections_update_count > buffer_state.last_selections_update_count;
            let buffer_diagnostics_updated =
                diagnostics_update_count > buffer_state.last_diagnostics_update_count;
            if buffer_edited
                || buffer_reparsed
                || buffer_selections_updated
                || buffer_diagnostics_updated
            {
                buffer_state.last_version = version;
                buffer_state.last_parse_count = parse_count;
                buffer_state.last_selections_update_count = selections_update_count;
                buffer_state.last_diagnostics_update_count = diagnostics_update_count;
                excerpts_to_edit.extend(
                    buffer_state
                        .excerpts
                        .iter()
                        .map(|excerpt_id| (excerpt_id, buffer_state.buffer.clone(), buffer_edited)),
                );
            }

            reparsed |= buffer_reparsed;
            diagnostics_updated |= buffer_diagnostics_updated;
            is_dirty |= buffer.is_dirty();
            has_conflict |= buffer.has_conflict();
        }
        if reparsed {
            snapshot.parse_count += 1;
        }
        if diagnostics_updated {
            snapshot.diagnostics_update_count += 1;
        }
        snapshot.is_dirty = is_dirty;
        snapshot.has_conflict = has_conflict;

        excerpts_to_edit.sort_unstable_by_key(|(excerpt_id, _, _)| *excerpt_id);

        let mut edits = Vec::new();
        let mut new_excerpts = SumTree::new();
        let mut cursor = snapshot.excerpts.cursor::<(Option<&ExcerptId>, usize)>();

        for (id, buffer, buffer_edited) in excerpts_to_edit {
            new_excerpts.push_tree(cursor.slice(&Some(id), Bias::Left, &()), &());
            let old_excerpt = cursor.item().unwrap();
            let buffer_id = buffer.id();
            let buffer = buffer.read(cx);

            let mut new_excerpt;
            if buffer_edited {
                edits.extend(
                    buffer
                        .edits_since_in_range::<usize>(
                            old_excerpt.buffer.version(),
                            old_excerpt.range.clone(),
                        )
                        .map(|mut edit| {
                            let excerpt_old_start = cursor.start().1;
                            let excerpt_new_start = new_excerpts.summary().text.bytes;
                            edit.old.start += excerpt_old_start;
                            edit.old.end += excerpt_old_start;
                            edit.new.start += excerpt_new_start;
                            edit.new.end += excerpt_new_start;
                            edit
                        }),
                );

                new_excerpt = Excerpt::new(
                    id.clone(),
                    buffer_id,
                    buffer.snapshot(),
                    old_excerpt.range.clone(),
                    old_excerpt.has_trailing_newline,
                );
            } else {
                new_excerpt = old_excerpt.clone();
                new_excerpt.buffer = buffer.snapshot();
            }

            new_excerpts.push(new_excerpt, &());
            cursor.next(&());
        }
        new_excerpts.push_tree(cursor.suffix(&()), &());

        drop(cursor);
        snapshot.excerpts = new_excerpts;

        self.subscriptions.publish(edits);
    }
}

#[cfg(any(test, feature = "test-support"))]
impl MultiBuffer {
    pub fn randomly_edit(
        &mut self,
        rng: &mut impl rand::Rng,
        count: usize,
        cx: &mut ModelContext<Self>,
    ) {
        use text::RandomCharIter;

        let snapshot = self.read(cx);
        let mut old_ranges: Vec<Range<usize>> = Vec::new();
        for _ in 0..count {
            let last_end = old_ranges.last().map_or(0, |last_range| last_range.end + 1);
            if last_end > snapshot.len() {
                break;
            }
            let end_ix = snapshot.clip_offset(rng.gen_range(0..=last_end), Bias::Right);
            let start_ix = snapshot.clip_offset(rng.gen_range(0..=end_ix), Bias::Left);
            old_ranges.push(start_ix..end_ix);
        }
        let new_text_len = rng.gen_range(0..10);
        let new_text: String = RandomCharIter::new(&mut *rng).take(new_text_len).collect();
        log::info!("mutating multi-buffer at {:?}: {:?}", old_ranges, new_text);
        drop(snapshot);

        self.edit(old_ranges.iter().cloned(), new_text.as_str(), cx);
    }
}

impl Entity for MultiBuffer {
    type Event = language::Event;
}

impl MultiBufferSnapshot {
    pub fn text(&self) -> String {
        self.chunks(0..self.len(), None)
            .map(|chunk| chunk.text)
            .collect()
    }

    pub fn reversed_chars_at<'a, T: ToOffset>(
        &'a self,
        position: T,
    ) -> impl Iterator<Item = char> + 'a {
        let mut offset = position.to_offset(self);
        let mut cursor = self.excerpts.cursor::<usize>();
        cursor.seek(&offset, Bias::Left, &());
        let mut excerpt_chunks = cursor.item().map(|excerpt| {
            let end_before_footer = cursor.start() + excerpt.text_summary.bytes;
            let start = excerpt.range.start.to_offset(&excerpt.buffer);
            let end = start + (cmp::min(offset, end_before_footer) - cursor.start());
            excerpt.buffer.reversed_chunks_in_range(start..end)
        });
        iter::from_fn(move || {
            if offset == *cursor.start() {
                cursor.prev(&());
                let excerpt = cursor.item()?;
                excerpt_chunks = Some(
                    excerpt
                        .buffer
                        .reversed_chunks_in_range(excerpt.range.clone()),
                );
            }

            let excerpt = cursor.item().unwrap();
            if offset == cursor.end(&()) && excerpt.has_trailing_newline {
                offset -= 1;
                Some("\n")
            } else {
                let chunk = excerpt_chunks.as_mut().unwrap().next().unwrap();
                offset -= chunk.len();
                Some(chunk)
            }
        })
        .flat_map(|c| c.chars().rev())
    }

    pub fn chars_at<'a, T: ToOffset>(&'a self, position: T) -> impl Iterator<Item = char> + 'a {
        let offset = position.to_offset(self);
        self.text_for_range(offset..self.len())
            .flat_map(|chunk| chunk.chars())
    }

    pub fn text_for_range<'a, T: ToOffset>(
        &'a self,
        range: Range<T>,
    ) -> impl Iterator<Item = &'a str> {
        self.chunks(range, None).map(|chunk| chunk.text)
    }

    pub fn is_line_blank(&self, row: u32) -> bool {
        self.text_for_range(Point::new(row, 0)..Point::new(row, self.line_len(row)))
            .all(|chunk| chunk.matches(|c: char| !c.is_whitespace()).next().is_none())
    }

    pub fn contains_str_at<T>(&self, position: T, needle: &str) -> bool
    where
        T: ToOffset,
    {
        let position = position.to_offset(self);
        position == self.clip_offset(position, Bias::Left)
            && self
                .bytes_in_range(position..self.len())
                .flatten()
                .copied()
                .take(needle.len())
                .eq(needle.bytes())
    }

    pub fn surrounding_word<T: ToOffset>(&self, start: T) -> (Range<usize>, Option<CharKind>) {
        let mut start = start.to_offset(self);
        let mut end = start;
        let mut next_chars = self.chars_at(start).peekable();
        let mut prev_chars = self.reversed_chars_at(start).peekable();
        let word_kind = cmp::max(
            prev_chars.peek().copied().map(char_kind),
            next_chars.peek().copied().map(char_kind),
        );

        for ch in prev_chars {
            if Some(char_kind(ch)) == word_kind {
                start -= ch.len_utf8();
            } else {
                break;
            }
        }

        for ch in next_chars {
            if Some(char_kind(ch)) == word_kind {
                end += ch.len_utf8();
            } else {
                break;
            }
        }

        (start..end, word_kind)
    }

    fn as_singleton(&self) -> Option<&Excerpt> {
        if self.singleton {
            self.excerpts.iter().next()
        } else {
            None
        }
    }

    pub fn len(&self) -> usize {
        self.excerpts.summary().text.bytes
    }

    pub fn max_buffer_row(&self) -> u32 {
        self.excerpts.summary().max_buffer_row
    }

    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        if let Some(excerpt) = self.as_singleton() {
            return excerpt.buffer.clip_offset(offset, bias);
        }

        let mut cursor = self.excerpts.cursor::<usize>();
        cursor.seek(&offset, Bias::Right, &());
        let overshoot = if let Some(excerpt) = cursor.item() {
            let excerpt_start = excerpt.range.start.to_offset(&excerpt.buffer);
            let buffer_offset = excerpt
                .buffer
                .clip_offset(excerpt_start + (offset - cursor.start()), bias);
            buffer_offset.saturating_sub(excerpt_start)
        } else {
            0
        };
        cursor.start() + overshoot
    }

    pub fn clip_point(&self, point: Point, bias: Bias) -> Point {
        if let Some(excerpt) = self.as_singleton() {
            return excerpt.buffer.clip_point(point, bias);
        }

        let mut cursor = self.excerpts.cursor::<Point>();
        cursor.seek(&point, Bias::Right, &());
        let overshoot = if let Some(excerpt) = cursor.item() {
            let excerpt_start = excerpt.range.start.to_point(&excerpt.buffer);
            let buffer_point = excerpt
                .buffer
                .clip_point(excerpt_start + (point - cursor.start()), bias);
            buffer_point.saturating_sub(excerpt_start)
        } else {
            Point::zero()
        };
        *cursor.start() + overshoot
    }

    pub fn clip_point_utf16(&self, point: PointUtf16, bias: Bias) -> PointUtf16 {
        if let Some(excerpt) = self.as_singleton() {
            return excerpt.buffer.clip_point_utf16(point, bias);
        }

        let mut cursor = self.excerpts.cursor::<PointUtf16>();
        cursor.seek(&point, Bias::Right, &());
        let overshoot = if let Some(excerpt) = cursor.item() {
            let excerpt_start = excerpt
                .buffer
                .offset_to_point_utf16(excerpt.range.start.to_offset(&excerpt.buffer));
            let buffer_point = excerpt
                .buffer
                .clip_point_utf16(excerpt_start + (point - cursor.start()), bias);
            buffer_point.saturating_sub(excerpt_start)
        } else {
            PointUtf16::zero()
        };
        *cursor.start() + overshoot
    }

    pub fn bytes_in_range<'a, T: ToOffset>(&'a self, range: Range<T>) -> MultiBufferBytes<'a> {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        let mut excerpts = self.excerpts.cursor::<usize>();
        excerpts.seek(&range.start, Bias::Right, &());

        let mut chunk = &[][..];
        let excerpt_bytes = if let Some(excerpt) = excerpts.item() {
            let mut excerpt_bytes = excerpt
                .bytes_in_range(range.start - excerpts.start()..range.end - excerpts.start());
            chunk = excerpt_bytes.next().unwrap_or(&[][..]);
            Some(excerpt_bytes)
        } else {
            None
        };

        MultiBufferBytes {
            range,
            excerpts,
            excerpt_bytes,
            chunk,
        }
    }

    pub fn buffer_rows<'a>(&'a self, start_row: u32) -> MultiBufferRows<'a> {
        let mut result = MultiBufferRows {
            buffer_row_range: 0..0,
            excerpts: self.excerpts.cursor(),
        };
        result.seek(start_row);
        result
    }

    pub fn chunks<'a, T: ToOffset>(
        &'a self,
        range: Range<T>,
        theme: Option<&'a SyntaxTheme>,
    ) -> MultiBufferChunks<'a> {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        let mut chunks = MultiBufferChunks {
            range: range.clone(),
            excerpts: self.excerpts.cursor(),
            excerpt_chunks: None,
            theme,
        };
        chunks.seek(range.start);
        chunks
    }

    pub fn offset_to_point(&self, offset: usize) -> Point {
        if let Some(excerpt) = self.as_singleton() {
            return excerpt.buffer.offset_to_point(offset);
        }

        let mut cursor = self.excerpts.cursor::<(usize, Point)>();
        cursor.seek(&offset, Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let (start_offset, start_point) = cursor.start();
            let overshoot = offset - start_offset;
            let excerpt_start_offset = excerpt.range.start.to_offset(&excerpt.buffer);
            let excerpt_start_point = excerpt.range.start.to_point(&excerpt.buffer);
            let buffer_point = excerpt
                .buffer
                .offset_to_point(excerpt_start_offset + overshoot);
            *start_point + (buffer_point - excerpt_start_point)
        } else {
            self.excerpts.summary().text.lines
        }
    }

    pub fn point_to_offset(&self, point: Point) -> usize {
        if let Some(excerpt) = self.as_singleton() {
            return excerpt.buffer.point_to_offset(point);
        }

        let mut cursor = self.excerpts.cursor::<(Point, usize)>();
        cursor.seek(&point, Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let (start_point, start_offset) = cursor.start();
            let overshoot = point - start_point;
            let excerpt_start_offset = excerpt.range.start.to_offset(&excerpt.buffer);
            let excerpt_start_point = excerpt.range.start.to_point(&excerpt.buffer);
            let buffer_offset = excerpt
                .buffer
                .point_to_offset(excerpt_start_point + overshoot);
            *start_offset + buffer_offset - excerpt_start_offset
        } else {
            self.excerpts.summary().text.bytes
        }
    }

    pub fn point_utf16_to_offset(&self, point: PointUtf16) -> usize {
        if let Some(excerpt) = self.as_singleton() {
            return excerpt.buffer.point_utf16_to_offset(point);
        }

        let mut cursor = self.excerpts.cursor::<(PointUtf16, usize)>();
        cursor.seek(&point, Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let (start_point, start_offset) = cursor.start();
            let overshoot = point - start_point;
            let excerpt_start_offset = excerpt.range.start.to_offset(&excerpt.buffer);
            let excerpt_start_point = excerpt
                .buffer
                .offset_to_point_utf16(excerpt.range.start.to_offset(&excerpt.buffer));
            let buffer_offset = excerpt
                .buffer
                .point_utf16_to_offset(excerpt_start_point + overshoot);
            *start_offset + (buffer_offset - excerpt_start_offset)
        } else {
            self.excerpts.summary().text.bytes
        }
    }

    pub fn indent_column_for_line(&self, row: u32) -> u32 {
        if let Some((buffer, range)) = self.buffer_line_for_row(row) {
            buffer
                .indent_column_for_line(range.start.row)
                .min(range.end.column)
                .saturating_sub(range.start.column)
        } else {
            0
        }
    }

    pub fn line_len(&self, row: u32) -> u32 {
        if let Some((_, range)) = self.buffer_line_for_row(row) {
            range.end.column - range.start.column
        } else {
            0
        }
    }

    fn buffer_line_for_row(&self, row: u32) -> Option<(&BufferSnapshot, Range<Point>)> {
        let mut cursor = self.excerpts.cursor::<Point>();
        cursor.seek(&Point::new(row, 0), Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let overshoot = row - cursor.start().row;
            let excerpt_start = excerpt.range.start.to_point(&excerpt.buffer);
            let excerpt_end = excerpt.range.end.to_point(&excerpt.buffer);
            let buffer_row = excerpt_start.row + overshoot;
            let line_start = Point::new(buffer_row, 0);
            let line_end = Point::new(buffer_row, excerpt.buffer.line_len(buffer_row));
            return Some((
                &excerpt.buffer,
                line_start.max(excerpt_start)..line_end.min(excerpt_end),
            ));
        }
        None
    }

    pub fn max_point(&self) -> Point {
        self.text_summary().lines
    }

    pub fn text_summary(&self) -> TextSummary {
        self.excerpts.summary().text
    }

    pub fn text_summary_for_range<'a, D, O>(&'a self, range: Range<O>) -> D
    where
        D: TextDimension,
        O: ToOffset,
    {
        let mut summary = D::default();
        let mut range = range.start.to_offset(self)..range.end.to_offset(self);
        let mut cursor = self.excerpts.cursor::<usize>();
        cursor.seek(&range.start, Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let mut end_before_newline = cursor.end(&());
            if excerpt.has_trailing_newline {
                end_before_newline -= 1;
            }

            let excerpt_start = excerpt.range.start.to_offset(&excerpt.buffer);
            let start_in_excerpt = excerpt_start + (range.start - cursor.start());
            let end_in_excerpt =
                excerpt_start + (cmp::min(end_before_newline, range.end) - cursor.start());
            summary.add_assign(
                &excerpt
                    .buffer
                    .text_summary_for_range(start_in_excerpt..end_in_excerpt),
            );

            if range.end > end_before_newline {
                summary.add_assign(&D::from_text_summary(&TextSummary {
                    bytes: 1,
                    lines: Point::new(1 as u32, 0),
                    lines_utf16: PointUtf16::new(1 as u32, 0),
                    first_line_chars: 0,
                    last_line_chars: 0,
                    longest_row: 0,
                    longest_row_chars: 0,
                }));
            }

            cursor.next(&());
        }

        if range.end > *cursor.start() {
            summary.add_assign(&D::from_text_summary(&cursor.summary::<_, TextSummary>(
                &range.end,
                Bias::Right,
                &(),
            )));
            if let Some(excerpt) = cursor.item() {
                range.end = cmp::max(*cursor.start(), range.end);

                let excerpt_start = excerpt.range.start.to_offset(&excerpt.buffer);
                let end_in_excerpt = excerpt_start + (range.end - cursor.start());
                summary.add_assign(
                    &excerpt
                        .buffer
                        .text_summary_for_range(excerpt_start..end_in_excerpt),
                );
            }
        }

        summary
    }

    pub fn summary_for_anchor<D>(&self, anchor: &Anchor) -> D
    where
        D: TextDimension + Ord + Sub<D, Output = D>,
    {
        let mut cursor = self.excerpts.cursor::<ExcerptSummary>();
        cursor.seek(&Some(&anchor.excerpt_id), Bias::Left, &());
        if cursor.item().is_none() {
            cursor.next(&());
        }

        let mut position = D::from_text_summary(&cursor.start().text);
        if let Some(excerpt) = cursor.item() {
            if excerpt.id == anchor.excerpt_id && excerpt.buffer_id == anchor.buffer_id {
                let excerpt_buffer_start = excerpt.range.start.summary::<D>(&excerpt.buffer);
                let excerpt_buffer_end = excerpt.range.end.summary::<D>(&excerpt.buffer);
                let buffer_position = cmp::min(
                    excerpt_buffer_end,
                    anchor.text_anchor.summary::<D>(&excerpt.buffer),
                );
                if buffer_position > excerpt_buffer_start {
                    position.add_assign(&(buffer_position - excerpt_buffer_start));
                }
            }
        }
        position
    }

    pub fn summaries_for_anchors<'a, D, I>(&'a self, anchors: I) -> Vec<D>
    where
        D: TextDimension + Ord + Sub<D, Output = D>,
        I: 'a + IntoIterator<Item = &'a Anchor>,
    {
        let mut anchors = anchors.into_iter().peekable();
        let mut cursor = self.excerpts.cursor::<ExcerptSummary>();
        let mut summaries = Vec::new();
        while let Some(anchor) = anchors.peek() {
            let excerpt_id = &anchor.excerpt_id;
            let buffer_id = anchor.buffer_id;
            let excerpt_anchors = iter::from_fn(|| {
                let anchor = anchors.peek()?;
                if anchor.excerpt_id == *excerpt_id && anchor.buffer_id == buffer_id {
                    Some(&anchors.next().unwrap().text_anchor)
                } else {
                    None
                }
            });

            cursor.seek_forward(&Some(excerpt_id), Bias::Left, &());
            if cursor.item().is_none() {
                cursor.next(&());
            }

            let position = D::from_text_summary(&cursor.start().text);
            if let Some(excerpt) = cursor.item() {
                if excerpt.id == *excerpt_id && excerpt.buffer_id == buffer_id {
                    let excerpt_buffer_start = excerpt.range.start.summary::<D>(&excerpt.buffer);
                    let excerpt_buffer_end = excerpt.range.end.summary::<D>(&excerpt.buffer);
                    summaries.extend(
                        excerpt
                            .buffer
                            .summaries_for_anchors::<D, _>(excerpt_anchors)
                            .map(move |summary| {
                                let summary = cmp::min(excerpt_buffer_end.clone(), summary);
                                let mut position = position.clone();
                                let excerpt_buffer_start = excerpt_buffer_start.clone();
                                if summary > excerpt_buffer_start {
                                    position.add_assign(&(summary - excerpt_buffer_start));
                                }
                                position
                            }),
                    );
                    continue;
                }
            }

            summaries.extend(excerpt_anchors.map(|_| position.clone()));
        }

        summaries
    }

    pub fn refresh_anchors<'a, I>(&'a self, anchors: I) -> Vec<(usize, Anchor, bool)>
    where
        I: 'a + IntoIterator<Item = &'a Anchor>,
    {
        let mut anchors = anchors.into_iter().enumerate().peekable();
        let mut cursor = self.excerpts.cursor::<Option<&ExcerptId>>();
        let mut result = Vec::new();
        while let Some((_, anchor)) = anchors.peek() {
            let old_excerpt_id = &anchor.excerpt_id;

            // Find the location where this anchor's excerpt should be.
            cursor.seek_forward(&Some(old_excerpt_id), Bias::Left, &());
            if cursor.item().is_none() {
                cursor.next(&());
            }

            let next_excerpt = cursor.item();
            let prev_excerpt = cursor.prev_item();

            // Process all of the anchors for this excerpt.
            while let Some((_, anchor)) = anchors.peek() {
                if anchor.excerpt_id != *old_excerpt_id {
                    break;
                }
                let mut kept_position = false;
                let (anchor_ix, anchor) = anchors.next().unwrap();
                let mut anchor = anchor.clone();

                // Leave min and max anchors unchanged.
                if *old_excerpt_id == ExcerptId::max() || *old_excerpt_id == ExcerptId::min() {
                    kept_position = true;
                }
                // If the old excerpt still exists at this location, then leave
                // the anchor unchanged.
                else if next_excerpt.map_or(false, |excerpt| {
                    excerpt.id == *old_excerpt_id && excerpt.contains(&anchor)
                }) {
                    kept_position = true;
                }
                // If the old excerpt no longer exists at this location, then attempt to
                // find an equivalent position for this anchor in an adjacent excerpt.
                else {
                    for excerpt in [next_excerpt, prev_excerpt].iter().filter_map(|e| *e) {
                        if excerpt.contains(&anchor) {
                            anchor.excerpt_id = excerpt.id.clone();
                            kept_position = true;
                            break;
                        }
                    }
                }
                // If there's no adjacent excerpt that contains the anchor's position,
                // then report that the anchor has lost its position.
                if !kept_position {
                    anchor = if let Some(excerpt) = next_excerpt {
                        let mut text_anchor = excerpt
                            .range
                            .start
                            .bias(anchor.text_anchor.bias, &excerpt.buffer);
                        if text_anchor
                            .cmp(&excerpt.range.end, &excerpt.buffer)
                            .unwrap()
                            .is_gt()
                        {
                            text_anchor = excerpt.range.end.clone();
                        }
                        Anchor {
                            buffer_id: excerpt.buffer_id,
                            excerpt_id: excerpt.id.clone(),
                            text_anchor,
                        }
                    } else if let Some(excerpt) = prev_excerpt {
                        let mut text_anchor = excerpt
                            .range
                            .end
                            .bias(anchor.text_anchor.bias, &excerpt.buffer);
                        if text_anchor
                            .cmp(&excerpt.range.start, &excerpt.buffer)
                            .unwrap()
                            .is_lt()
                        {
                            text_anchor = excerpt.range.start.clone();
                        }
                        Anchor {
                            buffer_id: excerpt.buffer_id,
                            excerpt_id: excerpt.id.clone(),
                            text_anchor,
                        }
                    } else if anchor.text_anchor.bias == Bias::Left {
                        Anchor::min()
                    } else {
                        Anchor::max()
                    };
                }

                result.push((anchor_ix, anchor, kept_position));
            }
        }
        result.sort_unstable_by(|a, b| a.1.cmp(&b.1, self).unwrap());
        result
    }

    pub fn anchor_before<T: ToOffset>(&self, position: T) -> Anchor {
        self.anchor_at(position, Bias::Left)
    }

    pub fn anchor_after<T: ToOffset>(&self, position: T) -> Anchor {
        self.anchor_at(position, Bias::Right)
    }

    pub fn anchor_at<T: ToOffset>(&self, position: T, mut bias: Bias) -> Anchor {
        let offset = position.to_offset(self);
        if let Some(excerpt) = self.as_singleton() {
            return Anchor {
                buffer_id: excerpt.buffer_id,
                excerpt_id: excerpt.id.clone(),
                text_anchor: excerpt.buffer.anchor_at(offset, bias),
            };
        }

        let mut cursor = self.excerpts.cursor::<(usize, Option<&ExcerptId>)>();
        cursor.seek(&offset, Bias::Right, &());
        if cursor.item().is_none() && offset == cursor.start().0 && bias == Bias::Left {
            cursor.prev(&());
        }
        if let Some(excerpt) = cursor.item() {
            let mut overshoot = offset.saturating_sub(cursor.start().0);
            if excerpt.has_trailing_newline && offset == cursor.end(&()).0 {
                overshoot -= 1;
                bias = Bias::Right;
            }

            let buffer_start = excerpt.range.start.to_offset(&excerpt.buffer);
            let text_anchor =
                excerpt.clip_anchor(excerpt.buffer.anchor_at(buffer_start + overshoot, bias));
            Anchor {
                buffer_id: excerpt.buffer_id,
                excerpt_id: excerpt.id.clone(),
                text_anchor,
            }
        } else if offset == 0 && bias == Bias::Left {
            Anchor::min()
        } else {
            Anchor::max()
        }
    }

    pub fn anchor_in_excerpt(&self, excerpt_id: ExcerptId, text_anchor: text::Anchor) -> Anchor {
        let mut cursor = self.excerpts.cursor::<Option<&ExcerptId>>();
        cursor.seek(&Some(&excerpt_id), Bias::Left, &());
        if let Some(excerpt) = cursor.item() {
            if excerpt.id == excerpt_id {
                let text_anchor = excerpt.clip_anchor(text_anchor);
                drop(cursor);
                return Anchor {
                    buffer_id: excerpt.buffer_id,
                    excerpt_id,
                    text_anchor,
                };
            }
        }
        panic!("excerpt not found");
    }

    pub fn can_resolve(&self, anchor: &Anchor) -> bool {
        if anchor.excerpt_id == ExcerptId::min() || anchor.excerpt_id == ExcerptId::max() {
            true
        } else if let Some((buffer_id, buffer_snapshot)) =
            self.buffer_snapshot_for_excerpt(&anchor.excerpt_id)
        {
            anchor.buffer_id == buffer_id && buffer_snapshot.can_resolve(&anchor.text_anchor)
        } else {
            false
        }
    }

    pub fn range_contains_excerpt_boundary<T: ToOffset>(&self, range: Range<T>) -> bool {
        let start = range.start.to_offset(self);
        let end = range.end.to_offset(self);
        let mut cursor = self.excerpts.cursor::<(usize, Option<&ExcerptId>)>();
        cursor.seek(&start, Bias::Right, &());
        let start_id = cursor
            .item()
            .or_else(|| cursor.prev_item())
            .map(|excerpt| &excerpt.id);
        cursor.seek_forward(&end, Bias::Right, &());
        let end_id = cursor
            .item()
            .or_else(|| cursor.prev_item())
            .map(|excerpt| &excerpt.id);
        start_id != end_id
    }

    pub fn parse_count(&self) -> usize {
        self.parse_count
    }

    pub fn enclosing_bracket_ranges<T: ToOffset>(
        &self,
        range: Range<T>,
    ) -> Option<(Range<usize>, Range<usize>)> {
        let range = range.start.to_offset(self)..range.end.to_offset(self);

        let mut cursor = self.excerpts.cursor::<usize>();
        cursor.seek(&range.start, Bias::Right, &());
        let start_excerpt = cursor.item();

        cursor.seek(&range.end, Bias::Right, &());
        let end_excerpt = cursor.item();

        start_excerpt
            .zip(end_excerpt)
            .and_then(|(start_excerpt, end_excerpt)| {
                if start_excerpt.id != end_excerpt.id {
                    return None;
                }

                let excerpt_buffer_start =
                    start_excerpt.range.start.to_offset(&start_excerpt.buffer);
                let excerpt_buffer_end = excerpt_buffer_start + start_excerpt.text_summary.bytes;

                let start_in_buffer =
                    excerpt_buffer_start + range.start.saturating_sub(*cursor.start());
                let end_in_buffer =
                    excerpt_buffer_start + range.end.saturating_sub(*cursor.start());
                let (mut start_bracket_range, mut end_bracket_range) = start_excerpt
                    .buffer
                    .enclosing_bracket_ranges(start_in_buffer..end_in_buffer)?;

                if start_bracket_range.start >= excerpt_buffer_start
                    && end_bracket_range.end < excerpt_buffer_end
                {
                    start_bracket_range.start =
                        cursor.start() + (start_bracket_range.start - excerpt_buffer_start);
                    start_bracket_range.end =
                        cursor.start() + (start_bracket_range.end - excerpt_buffer_start);
                    end_bracket_range.start =
                        cursor.start() + (end_bracket_range.start - excerpt_buffer_start);
                    end_bracket_range.end =
                        cursor.start() + (end_bracket_range.end - excerpt_buffer_start);
                    Some((start_bracket_range, end_bracket_range))
                } else {
                    None
                }
            })
    }

    pub fn diagnostics_update_count(&self) -> usize {
        self.diagnostics_update_count
    }

    pub fn language(&self) -> Option<&Arc<Language>> {
        self.excerpts
            .iter()
            .next()
            .and_then(|excerpt| excerpt.buffer.language())
    }

    pub fn is_dirty(&self) -> bool {
        self.is_dirty
    }

    pub fn has_conflict(&self) -> bool {
        self.has_conflict
    }

    pub fn diagnostic_group<'a, O>(
        &'a self,
        group_id: usize,
    ) -> impl Iterator<Item = DiagnosticEntry<O>> + 'a
    where
        O: text::FromAnchor + 'a,
    {
        self.as_singleton()
            .into_iter()
            .flat_map(move |excerpt| excerpt.buffer.diagnostic_group(group_id))
    }

    pub fn diagnostics_in_range<'a, T, O>(
        &'a self,
        range: Range<T>,
    ) -> impl Iterator<Item = DiagnosticEntry<O>> + 'a
    where
        T: 'a + ToOffset,
        O: 'a + text::FromAnchor,
    {
        self.as_singleton().into_iter().flat_map(move |excerpt| {
            excerpt
                .buffer
                .diagnostics_in_range(range.start.to_offset(self)..range.end.to_offset(self))
        })
    }

    pub fn range_for_syntax_ancestor<T: ToOffset>(&self, range: Range<T>) -> Option<Range<usize>> {
        let range = range.start.to_offset(self)..range.end.to_offset(self);

        let mut cursor = self.excerpts.cursor::<usize>();
        cursor.seek(&range.start, Bias::Right, &());
        let start_excerpt = cursor.item();

        cursor.seek(&range.end, Bias::Right, &());
        let end_excerpt = cursor.item();

        start_excerpt
            .zip(end_excerpt)
            .and_then(|(start_excerpt, end_excerpt)| {
                if start_excerpt.id != end_excerpt.id {
                    return None;
                }

                let excerpt_buffer_start =
                    start_excerpt.range.start.to_offset(&start_excerpt.buffer);
                let excerpt_buffer_end = excerpt_buffer_start + start_excerpt.text_summary.bytes;

                let start_in_buffer =
                    excerpt_buffer_start + range.start.saturating_sub(*cursor.start());
                let end_in_buffer =
                    excerpt_buffer_start + range.end.saturating_sub(*cursor.start());
                let mut ancestor_buffer_range = start_excerpt
                    .buffer
                    .range_for_syntax_ancestor(start_in_buffer..end_in_buffer)?;
                ancestor_buffer_range.start =
                    cmp::max(ancestor_buffer_range.start, excerpt_buffer_start);
                ancestor_buffer_range.end = cmp::min(ancestor_buffer_range.end, excerpt_buffer_end);

                let start = cursor.start() + (ancestor_buffer_range.start - excerpt_buffer_start);
                let end = cursor.start() + (ancestor_buffer_range.end - excerpt_buffer_start);
                Some(start..end)
            })
    }

    pub fn outline(&self, theme: Option<&SyntaxTheme>) -> Option<Outline<Anchor>> {
        let excerpt = self.as_singleton()?;
        let outline = excerpt.buffer.outline(theme)?;
        Some(Outline::new(
            outline
                .items
                .into_iter()
                .map(|item| OutlineItem {
                    depth: item.depth,
                    range: self.anchor_in_excerpt(excerpt.id.clone(), item.range.start)
                        ..self.anchor_in_excerpt(excerpt.id.clone(), item.range.end),
                    text: item.text,
                    highlight_ranges: item.highlight_ranges,
                    name_ranges: item.name_ranges,
                })
                .collect(),
        ))
    }

    fn buffer_snapshot_for_excerpt<'a>(
        &'a self,
        excerpt_id: &'a ExcerptId,
    ) -> Option<(usize, &'a BufferSnapshot)> {
        let mut cursor = self.excerpts.cursor::<Option<&ExcerptId>>();
        cursor.seek(&Some(excerpt_id), Bias::Left, &());
        if let Some(excerpt) = cursor.item() {
            if excerpt.id == *excerpt_id {
                return Some((excerpt.buffer_id, &excerpt.buffer));
            }
        }
        None
    }

    pub fn remote_selections_in_range<'a>(
        &'a self,
        range: &'a Range<Anchor>,
    ) -> impl 'a + Iterator<Item = (ReplicaId, Selection<Anchor>)> {
        let mut cursor = self.excerpts.cursor::<Option<&ExcerptId>>();
        cursor.seek(&Some(&range.start.excerpt_id), Bias::Left, &());
        cursor
            .take_while(move |excerpt| excerpt.id <= range.end.excerpt_id)
            .flat_map(move |excerpt| {
                let mut query_range = excerpt.range.start.clone()..excerpt.range.end.clone();
                if excerpt.id == range.start.excerpt_id {
                    query_range.start = range.start.text_anchor.clone();
                }
                if excerpt.id == range.end.excerpt_id {
                    query_range.end = range.end.text_anchor.clone();
                }

                excerpt
                    .buffer
                    .remote_selections_in_range(query_range)
                    .flat_map(move |(replica_id, selections)| {
                        selections.map(move |selection| {
                            let mut start = Anchor {
                                buffer_id: excerpt.buffer_id,
                                excerpt_id: excerpt.id.clone(),
                                text_anchor: selection.start.clone(),
                            };
                            let mut end = Anchor {
                                buffer_id: excerpt.buffer_id,
                                excerpt_id: excerpt.id.clone(),
                                text_anchor: selection.end.clone(),
                            };
                            if range.start.cmp(&start, self).unwrap().is_gt() {
                                start = range.start.clone();
                            }
                            if range.end.cmp(&end, self).unwrap().is_lt() {
                                end = range.end.clone();
                            }

                            (
                                replica_id,
                                Selection {
                                    id: selection.id,
                                    start,
                                    end,
                                    reversed: selection.reversed,
                                    goal: selection.goal,
                                },
                            )
                        })
                    })
            })
    }
}

impl History {
    fn start_transaction(&mut self, now: Instant) -> Option<TransactionId> {
        self.transaction_depth += 1;
        if self.transaction_depth == 1 {
            let id = post_inc(&mut self.next_transaction_id);
            self.undo_stack.push(Transaction {
                id,
                buffer_transactions: Default::default(),
                first_edit_at: now,
                last_edit_at: now,
            });
            Some(id)
        } else {
            None
        }
    }

    fn end_transaction(
        &mut self,
        now: Instant,
        buffer_transactions: HashSet<(usize, TransactionId)>,
    ) -> bool {
        assert_ne!(self.transaction_depth, 0);
        self.transaction_depth -= 1;
        if self.transaction_depth == 0 {
            if buffer_transactions.is_empty() {
                self.undo_stack.pop();
                false
            } else {
                let transaction = self.undo_stack.last_mut().unwrap();
                transaction.last_edit_at = now;
                transaction.buffer_transactions.extend(buffer_transactions);
                true
            }
        } else {
            false
        }
    }

    fn pop_undo(&mut self) -> Option<&Transaction> {
        assert_eq!(self.transaction_depth, 0);
        if let Some(transaction) = self.undo_stack.pop() {
            self.redo_stack.push(transaction);
            self.redo_stack.last()
        } else {
            None
        }
    }

    fn pop_redo(&mut self) -> Option<&Transaction> {
        assert_eq!(self.transaction_depth, 0);
        if let Some(transaction) = self.redo_stack.pop() {
            self.undo_stack.push(transaction);
            self.undo_stack.last()
        } else {
            None
        }
    }

    fn group(&mut self) -> Option<TransactionId> {
        let mut new_len = self.undo_stack.len();
        let mut transactions = self.undo_stack.iter_mut();

        if let Some(mut transaction) = transactions.next_back() {
            while let Some(prev_transaction) = transactions.next_back() {
                if transaction.first_edit_at - prev_transaction.last_edit_at <= self.group_interval
                {
                    transaction = prev_transaction;
                    new_len -= 1;
                } else {
                    break;
                }
            }
        }

        let (transactions_to_keep, transactions_to_merge) = self.undo_stack.split_at_mut(new_len);
        if let Some(last_transaction) = transactions_to_keep.last_mut() {
            if let Some(transaction) = transactions_to_merge.last() {
                last_transaction.last_edit_at = transaction.last_edit_at;
            }
        }

        self.undo_stack.truncate(new_len);
        self.undo_stack.last().map(|t| t.id)
    }
}

impl Excerpt {
    fn new(
        id: ExcerptId,
        buffer_id: usize,
        buffer: BufferSnapshot,
        range: Range<text::Anchor>,
        has_trailing_newline: bool,
    ) -> Self {
        Excerpt {
            id,
            max_buffer_row: range.end.to_point(&buffer).row,
            text_summary: buffer.text_summary_for_range::<TextSummary, _>(range.to_offset(&buffer)),
            buffer_id,
            buffer,
            range,
            has_trailing_newline,
        }
    }

    fn chunks_in_range<'a>(
        &'a self,
        range: Range<usize>,
        theme: Option<&'a SyntaxTheme>,
    ) -> ExcerptChunks<'a> {
        let content_start = self.range.start.to_offset(&self.buffer);
        let chunks_start = content_start + range.start;
        let chunks_end = content_start + cmp::min(range.end, self.text_summary.bytes);

        let footer_height = if self.has_trailing_newline
            && range.start <= self.text_summary.bytes
            && range.end > self.text_summary.bytes
        {
            1
        } else {
            0
        };

        let content_chunks = self.buffer.chunks(chunks_start..chunks_end, theme);

        ExcerptChunks {
            content_chunks,
            footer_height,
        }
    }

    fn bytes_in_range(&self, range: Range<usize>) -> ExcerptBytes {
        let content_start = self.range.start.to_offset(&self.buffer);
        let bytes_start = content_start + range.start;
        let bytes_end = content_start + cmp::min(range.end, self.text_summary.bytes);
        let footer_height = if self.has_trailing_newline
            && range.start <= self.text_summary.bytes
            && range.end > self.text_summary.bytes
        {
            1
        } else {
            0
        };
        let content_bytes = self.buffer.bytes_in_range(bytes_start..bytes_end);

        ExcerptBytes {
            content_bytes,
            footer_height,
        }
    }

    fn clip_anchor(&self, text_anchor: text::Anchor) -> text::Anchor {
        if text_anchor
            .cmp(&self.range.start, &self.buffer)
            .unwrap()
            .is_lt()
        {
            self.range.start.clone()
        } else if text_anchor
            .cmp(&self.range.end, &self.buffer)
            .unwrap()
            .is_gt()
        {
            self.range.end.clone()
        } else {
            text_anchor
        }
    }

    fn contains(&self, anchor: &Anchor) -> bool {
        self.buffer_id == anchor.buffer_id
            && self
                .range
                .start
                .cmp(&anchor.text_anchor, &self.buffer)
                .unwrap()
                .is_le()
            && self
                .range
                .end
                .cmp(&anchor.text_anchor, &self.buffer)
                .unwrap()
                .is_ge()
    }
}

impl fmt::Debug for Excerpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Excerpt")
            .field("id", &self.id)
            .field("buffer_id", &self.buffer_id)
            .field("range", &self.range)
            .field("text_summary", &self.text_summary)
            .field("has_trailing_newline", &self.has_trailing_newline)
            .finish()
    }
}

impl sum_tree::Item for Excerpt {
    type Summary = ExcerptSummary;

    fn summary(&self) -> Self::Summary {
        let mut text = self.text_summary.clone();
        if self.has_trailing_newline {
            text += TextSummary::from("\n");
        }
        ExcerptSummary {
            excerpt_id: self.id.clone(),
            max_buffer_row: self.max_buffer_row,
            text,
        }
    }
}

impl sum_tree::Summary for ExcerptSummary {
    type Context = ();

    fn add_summary(&mut self, summary: &Self, _: &()) {
        debug_assert!(summary.excerpt_id > self.excerpt_id);
        self.excerpt_id = summary.excerpt_id.clone();
        self.text.add_summary(&summary.text, &());
        self.max_buffer_row = cmp::max(self.max_buffer_row, summary.max_buffer_row);
    }
}

impl<'a> sum_tree::Dimension<'a, ExcerptSummary> for TextSummary {
    fn add_summary(&mut self, summary: &'a ExcerptSummary, _: &()) {
        *self += &summary.text;
    }
}

impl<'a> sum_tree::Dimension<'a, ExcerptSummary> for usize {
    fn add_summary(&mut self, summary: &'a ExcerptSummary, _: &()) {
        *self += summary.text.bytes;
    }
}

impl<'a> sum_tree::SeekTarget<'a, ExcerptSummary, ExcerptSummary> for usize {
    fn cmp(&self, cursor_location: &ExcerptSummary, _: &()) -> cmp::Ordering {
        Ord::cmp(self, &cursor_location.text.bytes)
    }
}

impl<'a> sum_tree::SeekTarget<'a, ExcerptSummary, ExcerptSummary> for Option<&'a ExcerptId> {
    fn cmp(&self, cursor_location: &ExcerptSummary, _: &()) -> cmp::Ordering {
        Ord::cmp(self, &Some(&cursor_location.excerpt_id))
    }
}

impl<'a> sum_tree::Dimension<'a, ExcerptSummary> for Point {
    fn add_summary(&mut self, summary: &'a ExcerptSummary, _: &()) {
        *self += summary.text.lines;
    }
}

impl<'a> sum_tree::Dimension<'a, ExcerptSummary> for PointUtf16 {
    fn add_summary(&mut self, summary: &'a ExcerptSummary, _: &()) {
        *self += summary.text.lines_utf16
    }
}

impl<'a> sum_tree::Dimension<'a, ExcerptSummary> for Option<&'a ExcerptId> {
    fn add_summary(&mut self, summary: &'a ExcerptSummary, _: &()) {
        *self = Some(&summary.excerpt_id);
    }
}

impl<'a> MultiBufferRows<'a> {
    pub fn seek(&mut self, row: u32) {
        self.buffer_row_range = 0..0;

        self.excerpts
            .seek_forward(&Point::new(row, 0), Bias::Right, &());
        if self.excerpts.item().is_none() {
            self.excerpts.prev(&());

            if self.excerpts.item().is_none() && row == 0 {
                self.buffer_row_range = 0..1;
                return;
            }
        }

        if let Some(excerpt) = self.excerpts.item() {
            let overshoot = row - self.excerpts.start().row;
            let excerpt_start = excerpt.range.start.to_point(&excerpt.buffer).row;
            self.buffer_row_range.start = excerpt_start + overshoot;
            self.buffer_row_range.end = excerpt_start + excerpt.text_summary.lines.row + 1;
        }
    }
}

impl<'a> Iterator for MultiBufferRows<'a> {
    type Item = Option<u32>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if !self.buffer_row_range.is_empty() {
                let row = Some(self.buffer_row_range.start);
                self.buffer_row_range.start += 1;
                return Some(row);
            }
            self.excerpts.item()?;
            self.excerpts.next(&());
            let excerpt = self.excerpts.item()?;
            self.buffer_row_range.start = excerpt.range.start.to_point(&excerpt.buffer).row;
            self.buffer_row_range.end =
                self.buffer_row_range.start + excerpt.text_summary.lines.row + 1;
        }
    }
}

impl<'a> MultiBufferChunks<'a> {
    pub fn offset(&self) -> usize {
        self.range.start
    }

    pub fn seek(&mut self, offset: usize) {
        self.range.start = offset;
        self.excerpts.seek(&offset, Bias::Right, &());
        if let Some(excerpt) = self.excerpts.item() {
            self.excerpt_chunks = Some(excerpt.chunks_in_range(
                self.range.start - self.excerpts.start()..self.range.end - self.excerpts.start(),
                self.theme,
            ));
        } else {
            self.excerpt_chunks = None;
        }
    }
}

impl<'a> Iterator for MultiBufferChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.range.is_empty() {
            None
        } else if let Some(chunk) = self.excerpt_chunks.as_mut()?.next() {
            self.range.start += chunk.text.len();
            Some(chunk)
        } else {
            self.excerpts.next(&());
            let excerpt = self.excerpts.item()?;
            self.excerpt_chunks = Some(
                excerpt.chunks_in_range(0..self.range.end - self.excerpts.start(), self.theme),
            );
            self.next()
        }
    }
}

impl<'a> MultiBufferBytes<'a> {
    fn consume(&mut self, len: usize) {
        self.range.start += len;
        self.chunk = &self.chunk[len..];

        if !self.range.is_empty() && self.chunk.is_empty() {
            if let Some(chunk) = self.excerpt_bytes.as_mut().and_then(|bytes| bytes.next()) {
                self.chunk = chunk;
            } else {
                self.excerpts.next(&());
                if let Some(excerpt) = self.excerpts.item() {
                    let mut excerpt_bytes =
                        excerpt.bytes_in_range(0..self.range.end - self.excerpts.start());
                    self.chunk = excerpt_bytes.next().unwrap();
                    self.excerpt_bytes = Some(excerpt_bytes);
                }
            }
        }
    }
}

impl<'a> Iterator for MultiBufferBytes<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let chunk = self.chunk;
        if chunk.is_empty() {
            None
        } else {
            self.consume(chunk.len());
            Some(chunk)
        }
    }
}

impl<'a> io::Read for MultiBufferBytes<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = cmp::min(buf.len(), self.chunk.len());
        buf[..len].copy_from_slice(&self.chunk[..len]);
        if len > 0 {
            self.consume(len);
        }
        Ok(len)
    }
}

impl<'a> Iterator for ExcerptBytes<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(chunk) = self.content_bytes.next() {
            if !chunk.is_empty() {
                return Some(chunk);
            }
        }

        if self.footer_height > 0 {
            let result = &NEWLINES[..self.footer_height];
            self.footer_height = 0;
            return Some(result);
        }

        None
    }
}

impl<'a> Iterator for ExcerptChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(chunk) = self.content_chunks.next() {
            return Some(chunk);
        }

        if self.footer_height > 0 {
            let text = unsafe { str::from_utf8_unchecked(&NEWLINES[..self.footer_height]) };
            self.footer_height = 0;
            return Some(Chunk {
                text,
                ..Default::default()
            });
        }

        None
    }
}

impl ToOffset for Point {
    fn to_offset<'a>(&self, snapshot: &MultiBufferSnapshot) -> usize {
        snapshot.point_to_offset(*self)
    }
}

impl ToOffset for PointUtf16 {
    fn to_offset<'a>(&self, snapshot: &MultiBufferSnapshot) -> usize {
        snapshot.point_utf16_to_offset(*self)
    }
}

impl ToOffset for usize {
    fn to_offset<'a>(&self, snapshot: &MultiBufferSnapshot) -> usize {
        assert!(*self <= snapshot.len(), "offset is out of range");
        *self
    }
}

impl ToPoint for usize {
    fn to_point<'a>(&self, snapshot: &MultiBufferSnapshot) -> Point {
        snapshot.offset_to_point(*self)
    }
}

impl ToPoint for Point {
    fn to_point<'a>(&self, _: &MultiBufferSnapshot) -> Point {
        *self
    }
}

pub fn char_kind(c: char) -> CharKind {
    if c == '\n' {
        CharKind::Newline
    } else if c.is_whitespace() {
        CharKind::Whitespace
    } else if c.is_alphanumeric() || c == '_' {
        CharKind::Word
    } else {
        CharKind::Punctuation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::MutableAppContext;
    use language::{Buffer, Rope};
    use rand::prelude::*;
    use std::env;
    use text::{Point, RandomCharIter};
    use util::test::sample_text;

    #[gpui::test]
    fn test_singleton_multibuffer(cx: &mut MutableAppContext) {
        let buffer = cx.add_model(|cx| Buffer::new(0, sample_text(6, 6, 'a'), cx));
        let multibuffer = cx.add_model(|cx| MultiBuffer::singleton(buffer.clone(), cx));

        let snapshot = multibuffer.read(cx).snapshot(cx);
        assert_eq!(snapshot.text(), buffer.read(cx).text());

        assert_eq!(
            snapshot.buffer_rows(0).collect::<Vec<_>>(),
            (0..buffer.read(cx).row_count())
                .map(Some)
                .collect::<Vec<_>>()
        );

        buffer.update(cx, |buffer, cx| buffer.edit([1..3], "XXX\n", cx));
        let snapshot = multibuffer.read(cx).snapshot(cx);

        assert_eq!(snapshot.text(), buffer.read(cx).text());
        assert_eq!(
            snapshot.buffer_rows(0).collect::<Vec<_>>(),
            (0..buffer.read(cx).row_count())
                .map(Some)
                .collect::<Vec<_>>()
        );
    }

    #[gpui::test]
    fn test_remote_multibuffer(cx: &mut MutableAppContext) {
        let host_buffer = cx.add_model(|cx| Buffer::new(0, "a", cx));
        let guest_buffer = cx.add_model(|cx| {
            let message = host_buffer.read(cx).to_proto();
            Buffer::from_proto(1, message, None, cx).unwrap()
        });
        let multibuffer = cx.add_model(|cx| MultiBuffer::singleton(guest_buffer.clone(), cx));
        let snapshot = multibuffer.read(cx).snapshot(cx);
        assert_eq!(snapshot.text(), "a");

        guest_buffer.update(cx, |buffer, cx| buffer.edit([1..1], "b", cx));
        let snapshot = multibuffer.read(cx).snapshot(cx);
        assert_eq!(snapshot.text(), "ab");

        guest_buffer.update(cx, |buffer, cx| buffer.edit([2..2], "c", cx));
        let snapshot = multibuffer.read(cx).snapshot(cx);
        assert_eq!(snapshot.text(), "abc");
    }

    #[gpui::test]
    fn test_excerpt_buffer(cx: &mut MutableAppContext) {
        let buffer_1 = cx.add_model(|cx| Buffer::new(0, sample_text(6, 6, 'a'), cx));
        let buffer_2 = cx.add_model(|cx| Buffer::new(0, sample_text(6, 6, 'g'), cx));
        let multibuffer = cx.add_model(|_| MultiBuffer::new(0));

        let subscription = multibuffer.update(cx, |multibuffer, cx| {
            let subscription = multibuffer.subscribe();
            multibuffer.push_excerpt(
                ExcerptProperties {
                    buffer: &buffer_1,
                    range: Point::new(1, 2)..Point::new(2, 5),
                },
                cx,
            );
            assert_eq!(
                subscription.consume().into_inner(),
                [Edit {
                    old: 0..0,
                    new: 0..10
                }]
            );

            multibuffer.push_excerpt(
                ExcerptProperties {
                    buffer: &buffer_1,
                    range: Point::new(3, 3)..Point::new(4, 4),
                },
                cx,
            );
            multibuffer.push_excerpt(
                ExcerptProperties {
                    buffer: &buffer_2,
                    range: Point::new(3, 1)..Point::new(3, 3),
                },
                cx,
            );
            assert_eq!(
                subscription.consume().into_inner(),
                [Edit {
                    old: 10..10,
                    new: 10..22
                }]
            );

            subscription
        });

        let snapshot = multibuffer.read(cx).snapshot(cx);
        assert_eq!(
            snapshot.text(),
            concat!(
                "bbbb\n",  // Preserve newlines
                "ccccc\n", //
                "ddd\n",   //
                "eeee\n",  //
                "jj"       //
            )
        );
        assert_eq!(
            snapshot.buffer_rows(0).collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3), Some(4), Some(3)]
        );
        assert_eq!(
            snapshot.buffer_rows(2).collect::<Vec<_>>(),
            [Some(3), Some(4), Some(3)]
        );
        assert_eq!(snapshot.buffer_rows(4).collect::<Vec<_>>(), [Some(3)]);
        assert_eq!(snapshot.buffer_rows(5).collect::<Vec<_>>(), []);
        assert!(!snapshot.range_contains_excerpt_boundary(Point::new(1, 0)..Point::new(1, 5)));
        assert!(snapshot.range_contains_excerpt_boundary(Point::new(1, 0)..Point::new(2, 0)));
        assert!(snapshot.range_contains_excerpt_boundary(Point::new(1, 0)..Point::new(4, 0)));
        assert!(!snapshot.range_contains_excerpt_boundary(Point::new(2, 0)..Point::new(3, 0)));
        assert!(!snapshot.range_contains_excerpt_boundary(Point::new(4, 0)..Point::new(4, 2)));
        assert!(!snapshot.range_contains_excerpt_boundary(Point::new(4, 2)..Point::new(4, 2)));

        buffer_1.update(cx, |buffer, cx| {
            buffer.edit(
                [
                    Point::new(0, 0)..Point::new(0, 0),
                    Point::new(2, 1)..Point::new(2, 3),
                ],
                "\n",
                cx,
            );
        });

        let snapshot = multibuffer.read(cx).snapshot(cx);
        assert_eq!(
            snapshot.text(),
            concat!(
                "bbbb\n", // Preserve newlines
                "c\n",    //
                "cc\n",   //
                "ddd\n",  //
                "eeee\n", //
                "jj"      //
            )
        );

        assert_eq!(
            subscription.consume().into_inner(),
            [Edit {
                old: 6..8,
                new: 6..7
            }]
        );

        let snapshot = multibuffer.read(cx).snapshot(cx);
        assert_eq!(
            snapshot.clip_point(Point::new(0, 5), Bias::Left),
            Point::new(0, 4)
        );
        assert_eq!(
            snapshot.clip_point(Point::new(0, 5), Bias::Right),
            Point::new(0, 4)
        );
        assert_eq!(
            snapshot.clip_point(Point::new(5, 1), Bias::Right),
            Point::new(5, 1)
        );
        assert_eq!(
            snapshot.clip_point(Point::new(5, 2), Bias::Right),
            Point::new(5, 2)
        );
        assert_eq!(
            snapshot.clip_point(Point::new(5, 3), Bias::Right),
            Point::new(5, 2)
        );

        let snapshot = multibuffer.update(cx, |multibuffer, cx| {
            let buffer_2_excerpt_id = multibuffer.excerpt_ids_for_buffer(&buffer_2)[0].clone();
            multibuffer.remove_excerpts(&[buffer_2_excerpt_id], cx);
            multibuffer.snapshot(cx)
        });

        assert_eq!(
            snapshot.text(),
            concat!(
                "bbbb\n", // Preserve newlines
                "c\n",    //
                "cc\n",   //
                "ddd\n",  //
                "eeee",   //
            )
        );
    }

    #[gpui::test]
    fn test_empty_excerpt_buffer(cx: &mut MutableAppContext) {
        let multibuffer = cx.add_model(|_| MultiBuffer::new(0));

        let snapshot = multibuffer.read(cx).snapshot(cx);
        assert_eq!(snapshot.text(), "");
        assert_eq!(snapshot.buffer_rows(0).collect::<Vec<_>>(), &[Some(0)]);
        assert_eq!(snapshot.buffer_rows(1).collect::<Vec<_>>(), &[]);
    }

    #[gpui::test]
    fn test_singleton_multibuffer_anchors(cx: &mut MutableAppContext) {
        let buffer = cx.add_model(|cx| Buffer::new(0, "abcd", cx));
        let multibuffer = cx.add_model(|cx| MultiBuffer::singleton(buffer.clone(), cx));
        let old_snapshot = multibuffer.read(cx).snapshot(cx);
        buffer.update(cx, |buffer, cx| {
            buffer.edit([0..0], "X", cx);
            buffer.edit([5..5], "Y", cx);
        });
        let new_snapshot = multibuffer.read(cx).snapshot(cx);

        assert_eq!(old_snapshot.text(), "abcd");
        assert_eq!(new_snapshot.text(), "XabcdY");

        assert_eq!(old_snapshot.anchor_before(0).to_offset(&new_snapshot), 0);
        assert_eq!(old_snapshot.anchor_after(0).to_offset(&new_snapshot), 1);
        assert_eq!(old_snapshot.anchor_before(4).to_offset(&new_snapshot), 5);
        assert_eq!(old_snapshot.anchor_after(4).to_offset(&new_snapshot), 6);
    }

    #[gpui::test]
    fn test_multibuffer_anchors(cx: &mut MutableAppContext) {
        let buffer_1 = cx.add_model(|cx| Buffer::new(0, "abcd", cx));
        let buffer_2 = cx.add_model(|cx| Buffer::new(0, "efghi", cx));
        let multibuffer = cx.add_model(|cx| {
            let mut multibuffer = MultiBuffer::new(0);
            multibuffer.push_excerpt(
                ExcerptProperties {
                    buffer: &buffer_1,
                    range: 0..4,
                },
                cx,
            );
            multibuffer.push_excerpt(
                ExcerptProperties {
                    buffer: &buffer_2,
                    range: 0..5,
                },
                cx,
            );
            multibuffer
        });
        let old_snapshot = multibuffer.read(cx).snapshot(cx);

        assert_eq!(old_snapshot.anchor_before(0).to_offset(&old_snapshot), 0);
        assert_eq!(old_snapshot.anchor_after(0).to_offset(&old_snapshot), 0);
        assert_eq!(Anchor::min().to_offset(&old_snapshot), 0);
        assert_eq!(Anchor::min().to_offset(&old_snapshot), 0);
        assert_eq!(Anchor::max().to_offset(&old_snapshot), 10);
        assert_eq!(Anchor::max().to_offset(&old_snapshot), 10);

        buffer_1.update(cx, |buffer, cx| {
            buffer.edit([0..0], "W", cx);
            buffer.edit([5..5], "X", cx);
        });
        buffer_2.update(cx, |buffer, cx| {
            buffer.edit([0..0], "Y", cx);
            buffer.edit([6..0], "Z", cx);
        });
        let new_snapshot = multibuffer.read(cx).snapshot(cx);

        assert_eq!(old_snapshot.text(), "abcd\nefghi");
        assert_eq!(new_snapshot.text(), "WabcdX\nYefghiZ");

        assert_eq!(old_snapshot.anchor_before(0).to_offset(&new_snapshot), 0);
        assert_eq!(old_snapshot.anchor_after(0).to_offset(&new_snapshot), 1);
        assert_eq!(old_snapshot.anchor_before(1).to_offset(&new_snapshot), 2);
        assert_eq!(old_snapshot.anchor_after(1).to_offset(&new_snapshot), 2);
        assert_eq!(old_snapshot.anchor_before(2).to_offset(&new_snapshot), 3);
        assert_eq!(old_snapshot.anchor_after(2).to_offset(&new_snapshot), 3);
        assert_eq!(old_snapshot.anchor_before(5).to_offset(&new_snapshot), 7);
        assert_eq!(old_snapshot.anchor_after(5).to_offset(&new_snapshot), 8);
        assert_eq!(old_snapshot.anchor_before(10).to_offset(&new_snapshot), 13);
        assert_eq!(old_snapshot.anchor_after(10).to_offset(&new_snapshot), 14);
    }

    #[gpui::test]
    fn test_multibuffer_resolving_anchors_after_replacing_their_excerpts(
        cx: &mut MutableAppContext,
    ) {
        let buffer_1 = cx.add_model(|cx| Buffer::new(0, "abcd", cx));
        let buffer_2 = cx.add_model(|cx| Buffer::new(0, "ABCDEFGHIJKLMNOP", cx));
        let multibuffer = cx.add_model(|_| MultiBuffer::new(0));

        // Create an insertion id in buffer 1 that doesn't exist in buffer 2.
        // Add an excerpt from buffer 1 that spans this new insertion.
        buffer_1.update(cx, |buffer, cx| buffer.edit([4..4], "123", cx));
        let excerpt_id_1 = multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.push_excerpt(
                ExcerptProperties {
                    buffer: &buffer_1,
                    range: 0..7,
                },
                cx,
            )
        });

        let snapshot_1 = multibuffer.read(cx).snapshot(cx);
        assert_eq!(snapshot_1.text(), "abcd123");

        // Replace the buffer 1 excerpt with new excerpts from buffer 2.
        let (excerpt_id_2, excerpt_id_3, _) = multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.remove_excerpts([&excerpt_id_1], cx);
            (
                multibuffer.push_excerpt(
                    ExcerptProperties {
                        buffer: &buffer_2,
                        range: 0..4,
                    },
                    cx,
                ),
                multibuffer.push_excerpt(
                    ExcerptProperties {
                        buffer: &buffer_2,
                        range: 6..10,
                    },
                    cx,
                ),
                multibuffer.push_excerpt(
                    ExcerptProperties {
                        buffer: &buffer_2,
                        range: 12..16,
                    },
                    cx,
                ),
            )
        });
        let snapshot_2 = multibuffer.read(cx).snapshot(cx);
        assert_eq!(snapshot_2.text(), "ABCD\nGHIJ\nMNOP");

        // The old excerpt id has been reused.
        assert_eq!(excerpt_id_2, excerpt_id_1);

        // Resolve some anchors from the previous snapshot in the new snapshot.
        // Although there is still an excerpt with the same id, it is for
        // a different buffer, so we don't attempt to resolve the old text
        // anchor in the new buffer.
        assert_eq!(
            snapshot_2.summary_for_anchor::<usize>(&snapshot_1.anchor_before(2)),
            0
        );
        assert_eq!(
            snapshot_2.summaries_for_anchors::<usize, _>(&[
                snapshot_1.anchor_before(2),
                snapshot_1.anchor_after(3)
            ]),
            vec![0, 0]
        );
        let refresh =
            snapshot_2.refresh_anchors(&[snapshot_1.anchor_before(2), snapshot_1.anchor_after(3)]);
        assert_eq!(
            refresh,
            &[
                (0, snapshot_2.anchor_before(0), false),
                (1, snapshot_2.anchor_after(0), false),
            ]
        );

        // Replace the middle excerpt with a smaller excerpt in buffer 2,
        // that intersects the old excerpt.
        let excerpt_id_5 = multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.remove_excerpts([&excerpt_id_3], cx);
            multibuffer.insert_excerpt_after(
                &excerpt_id_3,
                ExcerptProperties {
                    buffer: &buffer_2,
                    range: 5..8,
                },
                cx,
            )
        });

        let snapshot_3 = multibuffer.read(cx).snapshot(cx);
        assert_eq!(snapshot_3.text(), "ABCD\nFGH\nMNOP");
        assert_ne!(excerpt_id_5, excerpt_id_3);

        // Resolve some anchors from the previous snapshot in the new snapshot.
        // The anchor in the middle excerpt snaps to the beginning of the
        // excerpt, since it is not
        let anchors = [
            snapshot_2.anchor_before(0),
            snapshot_2.anchor_after(2),
            snapshot_2.anchor_after(6),
            snapshot_2.anchor_after(14),
        ];
        assert_eq!(
            snapshot_3.summaries_for_anchors::<usize, _>(&anchors),
            &[0, 2, 9, 13]
        );

        let new_anchors = snapshot_3.refresh_anchors(&anchors);
        assert_eq!(
            new_anchors.iter().map(|a| (a.0, a.2)).collect::<Vec<_>>(),
            &[(0, true), (1, true), (2, true), (3, true)]
        );
        assert_eq!(
            snapshot_3.summaries_for_anchors::<usize, _>(new_anchors.iter().map(|a| &a.1)),
            &[0, 2, 7, 13]
        );
    }

    #[gpui::test(iterations = 100)]
    fn test_random_multibuffer(cx: &mut MutableAppContext, mut rng: StdRng) {
        let operations = env::var("OPERATIONS")
            .map(|i| i.parse().expect("invalid `OPERATIONS` variable"))
            .unwrap_or(10);

        let mut buffers: Vec<ModelHandle<Buffer>> = Vec::new();
        let multibuffer = cx.add_model(|_| MultiBuffer::new(0));
        let mut excerpt_ids = Vec::new();
        let mut expected_excerpts = Vec::<(ModelHandle<Buffer>, Range<text::Anchor>)>::new();
        let mut anchors = Vec::new();
        let mut old_versions = Vec::new();

        for _ in 0..operations {
            match rng.gen_range(0..100) {
                0..=19 if !buffers.is_empty() => {
                    let buffer = buffers.choose(&mut rng).unwrap();
                    buffer.update(cx, |buf, cx| buf.randomly_edit(&mut rng, 5, cx));
                }
                20..=29 if !expected_excerpts.is_empty() => {
                    let mut ids_to_remove = vec![];
                    for _ in 0..rng.gen_range(1..=3) {
                        if expected_excerpts.is_empty() {
                            break;
                        }

                        let ix = rng.gen_range(0..expected_excerpts.len());
                        ids_to_remove.push(excerpt_ids.remove(ix));
                        let (buffer, range) = expected_excerpts.remove(ix);
                        let buffer = buffer.read(cx);
                        log::info!(
                            "Removing excerpt {}: {:?}",
                            ix,
                            buffer
                                .text_for_range(range.to_offset(&buffer))
                                .collect::<String>(),
                        );
                    }
                    ids_to_remove.sort_unstable();
                    multibuffer.update(cx, |multibuffer, cx| {
                        multibuffer.remove_excerpts(&ids_to_remove, cx)
                    });
                }
                30..=39 if !expected_excerpts.is_empty() => {
                    let multibuffer = multibuffer.read(cx).read(cx);
                    let offset =
                        multibuffer.clip_offset(rng.gen_range(0..=multibuffer.len()), Bias::Left);
                    let bias = if rng.gen() { Bias::Left } else { Bias::Right };
                    log::info!("Creating anchor at {} with bias {:?}", offset, bias);
                    anchors.push(multibuffer.anchor_at(offset, bias));
                    anchors.sort_by(|a, b| a.cmp(&b, &multibuffer).unwrap());
                }
                40..=44 if !anchors.is_empty() => {
                    let multibuffer = multibuffer.read(cx).read(cx);

                    anchors = multibuffer
                        .refresh_anchors(&anchors)
                        .into_iter()
                        .map(|a| a.1)
                        .collect();

                    // Ensure the newly-refreshed anchors point to a valid excerpt and don't
                    // overshoot its boundaries.
                    let mut cursor = multibuffer.excerpts.cursor::<Option<&ExcerptId>>();
                    for anchor in &anchors {
                        if anchor.excerpt_id == ExcerptId::min()
                            || anchor.excerpt_id == ExcerptId::max()
                        {
                            continue;
                        }

                        cursor.seek_forward(&Some(&anchor.excerpt_id), Bias::Left, &());
                        let excerpt = cursor.item().unwrap();
                        assert_eq!(excerpt.id, anchor.excerpt_id);
                        assert!(excerpt.contains(anchor));
                    }
                }
                _ => {
                    let buffer_handle = if buffers.is_empty() || rng.gen_bool(0.4) {
                        let base_text = RandomCharIter::new(&mut rng).take(10).collect::<String>();
                        buffers.push(cx.add_model(|cx| Buffer::new(0, base_text, cx)));
                        buffers.last().unwrap()
                    } else {
                        buffers.choose(&mut rng).unwrap()
                    };

                    let buffer = buffer_handle.read(cx);
                    let end_ix = buffer.clip_offset(rng.gen_range(0..=buffer.len()), Bias::Right);
                    let start_ix = buffer.clip_offset(rng.gen_range(0..=end_ix), Bias::Left);
                    let anchor_range = buffer.anchor_before(start_ix)..buffer.anchor_after(end_ix);
                    let prev_excerpt_ix = rng.gen_range(0..=expected_excerpts.len());
                    let prev_excerpt_id = excerpt_ids
                        .get(prev_excerpt_ix)
                        .cloned()
                        .unwrap_or(ExcerptId::max());
                    let excerpt_ix = (prev_excerpt_ix + 1).min(expected_excerpts.len());

                    log::info!(
                        "Inserting excerpt at {} of {} for buffer {}: {:?}[{:?}] = {:?}",
                        excerpt_ix,
                        expected_excerpts.len(),
                        buffer_handle.id(),
                        buffer.text(),
                        start_ix..end_ix,
                        &buffer.text()[start_ix..end_ix]
                    );

                    let excerpt_id = multibuffer.update(cx, |multibuffer, cx| {
                        multibuffer.insert_excerpt_after(
                            &prev_excerpt_id,
                            ExcerptProperties {
                                buffer: &buffer_handle,
                                range: start_ix..end_ix,
                            },
                            cx,
                        )
                    });

                    excerpt_ids.insert(excerpt_ix, excerpt_id);
                    expected_excerpts.insert(excerpt_ix, (buffer_handle.clone(), anchor_range));
                }
            }

            if rng.gen_bool(0.3) {
                multibuffer.update(cx, |multibuffer, cx| {
                    old_versions.push((multibuffer.snapshot(cx), multibuffer.subscribe()));
                })
            }

            let snapshot = multibuffer.read(cx).snapshot(cx);

            let mut excerpt_starts = Vec::new();
            let mut expected_text = String::new();
            let mut expected_buffer_rows = Vec::new();
            for (buffer, range) in &expected_excerpts {
                let buffer = buffer.read(cx);
                let buffer_range = range.to_offset(buffer);

                excerpt_starts.push(TextSummary::from(expected_text.as_str()));
                expected_text.extend(buffer.text_for_range(buffer_range.clone()));
                expected_text.push('\n');

                let buffer_row_range = buffer.offset_to_point(buffer_range.start).row
                    ..=buffer.offset_to_point(buffer_range.end).row;
                for row in buffer_row_range {
                    expected_buffer_rows.push(Some(row));
                }
            }
            // Remove final trailing newline.
            if !expected_excerpts.is_empty() {
                expected_text.pop();
            }

            // Always report one buffer row
            if expected_buffer_rows.is_empty() {
                expected_buffer_rows.push(Some(0));
            }

            assert_eq!(snapshot.text(), expected_text);
            log::info!("MultiBuffer text: {:?}", expected_text);

            assert_eq!(
                snapshot.buffer_rows(0).collect::<Vec<_>>(),
                expected_buffer_rows,
            );

            for _ in 0..5 {
                let start_row = rng.gen_range(0..=expected_buffer_rows.len());
                assert_eq!(
                    snapshot.buffer_rows(start_row as u32).collect::<Vec<_>>(),
                    &expected_buffer_rows[start_row..],
                    "buffer_rows({})",
                    start_row
                );
            }

            assert_eq!(
                snapshot.max_buffer_row(),
                expected_buffer_rows
                    .into_iter()
                    .filter_map(|r| r)
                    .max()
                    .unwrap()
            );

            let mut excerpt_starts = excerpt_starts.into_iter();
            for (buffer, range) in &expected_excerpts {
                let buffer_id = buffer.id();
                let buffer = buffer.read(cx);
                let buffer_range = range.to_offset(buffer);
                let buffer_start_point = buffer.offset_to_point(buffer_range.start);
                let buffer_start_point_utf16 =
                    buffer.text_summary_for_range::<PointUtf16, _>(0..buffer_range.start);

                let excerpt_start = excerpt_starts.next().unwrap();
                let mut offset = excerpt_start.bytes;
                let mut buffer_offset = buffer_range.start;
                let mut point = excerpt_start.lines;
                let mut buffer_point = buffer_start_point;
                let mut point_utf16 = excerpt_start.lines_utf16;
                let mut buffer_point_utf16 = buffer_start_point_utf16;
                for ch in buffer
                    .snapshot()
                    .chunks(buffer_range.clone(), None)
                    .flat_map(|c| c.text.chars())
                {
                    for _ in 0..ch.len_utf8() {
                        let left_offset = snapshot.clip_offset(offset, Bias::Left);
                        let right_offset = snapshot.clip_offset(offset, Bias::Right);
                        let buffer_left_offset = buffer.clip_offset(buffer_offset, Bias::Left);
                        let buffer_right_offset = buffer.clip_offset(buffer_offset, Bias::Right);
                        assert_eq!(
                            left_offset,
                            excerpt_start.bytes + (buffer_left_offset - buffer_range.start),
                            "clip_offset({:?}, Left). buffer: {:?}, buffer offset: {:?}",
                            offset,
                            buffer_id,
                            buffer_offset,
                        );
                        assert_eq!(
                            right_offset,
                            excerpt_start.bytes + (buffer_right_offset - buffer_range.start),
                            "clip_offset({:?}, Right). buffer: {:?}, buffer offset: {:?}",
                            offset,
                            buffer_id,
                            buffer_offset,
                        );

                        let left_point = snapshot.clip_point(point, Bias::Left);
                        let right_point = snapshot.clip_point(point, Bias::Right);
                        let buffer_left_point = buffer.clip_point(buffer_point, Bias::Left);
                        let buffer_right_point = buffer.clip_point(buffer_point, Bias::Right);
                        assert_eq!(
                            left_point,
                            excerpt_start.lines + (buffer_left_point - buffer_start_point),
                            "clip_point({:?}, Left). buffer: {:?}, buffer point: {:?}",
                            point,
                            buffer_id,
                            buffer_point,
                        );
                        assert_eq!(
                            right_point,
                            excerpt_start.lines + (buffer_right_point - buffer_start_point),
                            "clip_point({:?}, Right). buffer: {:?}, buffer point: {:?}",
                            point,
                            buffer_id,
                            buffer_point,
                        );

                        assert_eq!(
                            snapshot.point_to_offset(left_point),
                            left_offset,
                            "point_to_offset({:?})",
                            left_point,
                        );
                        assert_eq!(
                            snapshot.offset_to_point(left_offset),
                            left_point,
                            "offset_to_point({:?})",
                            left_offset,
                        );

                        offset += 1;
                        buffer_offset += 1;
                        if ch == '\n' {
                            point += Point::new(1, 0);
                            buffer_point += Point::new(1, 0);
                        } else {
                            point += Point::new(0, 1);
                            buffer_point += Point::new(0, 1);
                        }
                    }

                    for _ in 0..ch.len_utf16() {
                        let left_point_utf16 = snapshot.clip_point_utf16(point_utf16, Bias::Left);
                        let right_point_utf16 = snapshot.clip_point_utf16(point_utf16, Bias::Right);
                        let buffer_left_point_utf16 =
                            buffer.clip_point_utf16(buffer_point_utf16, Bias::Left);
                        let buffer_right_point_utf16 =
                            buffer.clip_point_utf16(buffer_point_utf16, Bias::Right);
                        assert_eq!(
                            left_point_utf16,
                            excerpt_start.lines_utf16
                                + (buffer_left_point_utf16 - buffer_start_point_utf16),
                            "clip_point_utf16({:?}, Left). buffer: {:?}, buffer point_utf16: {:?}",
                            point_utf16,
                            buffer_id,
                            buffer_point_utf16,
                        );
                        assert_eq!(
                            right_point_utf16,
                            excerpt_start.lines_utf16
                                + (buffer_right_point_utf16 - buffer_start_point_utf16),
                            "clip_point_utf16({:?}, Right). buffer: {:?}, buffer point_utf16: {:?}",
                            point_utf16,
                            buffer_id,
                            buffer_point_utf16,
                        );

                        if ch == '\n' {
                            point_utf16 += PointUtf16::new(1, 0);
                            buffer_point_utf16 += PointUtf16::new(1, 0);
                        } else {
                            point_utf16 += PointUtf16::new(0, 1);
                            buffer_point_utf16 += PointUtf16::new(0, 1);
                        }
                    }
                }
            }

            for (row, line) in expected_text.split('\n').enumerate() {
                assert_eq!(
                    snapshot.line_len(row as u32),
                    line.len() as u32,
                    "line_len({}).",
                    row
                );
            }

            let text_rope = Rope::from(expected_text.as_str());
            for _ in 0..10 {
                let end_ix = text_rope.clip_offset(rng.gen_range(0..=text_rope.len()), Bias::Right);
                let start_ix = text_rope.clip_offset(rng.gen_range(0..=end_ix), Bias::Left);

                let text_for_range = snapshot
                    .text_for_range(start_ix..end_ix)
                    .collect::<String>();
                assert_eq!(
                    text_for_range,
                    &expected_text[start_ix..end_ix],
                    "incorrect text for range {:?}",
                    start_ix..end_ix
                );

                let excerpted_buffer_ranges =
                    multibuffer.read(cx).excerpted_buffers(start_ix..end_ix, cx);
                let excerpted_buffers_text = excerpted_buffer_ranges
                    .into_iter()
                    .map(|(buffer, buffer_range)| {
                        buffer
                            .read(cx)
                            .text_for_range(buffer_range)
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert_eq!(excerpted_buffers_text, text_for_range);

                let expected_summary = TextSummary::from(&expected_text[start_ix..end_ix]);
                assert_eq!(
                    snapshot.text_summary_for_range::<TextSummary, _>(start_ix..end_ix),
                    expected_summary,
                    "incorrect summary for range {:?}",
                    start_ix..end_ix
                );
            }

            // Anchor resolution
            for (anchor, resolved_offset) in anchors
                .iter()
                .zip(snapshot.summaries_for_anchors::<usize, _>(&anchors))
            {
                assert!(resolved_offset <= snapshot.len());
                assert_eq!(
                    snapshot.summary_for_anchor::<usize>(anchor),
                    resolved_offset
                );
            }

            for _ in 0..10 {
                let end_ix = text_rope.clip_offset(rng.gen_range(0..=text_rope.len()), Bias::Right);
                assert_eq!(
                    snapshot.reversed_chars_at(end_ix).collect::<String>(),
                    expected_text[..end_ix].chars().rev().collect::<String>(),
                );
            }

            for _ in 0..10 {
                let end_ix = rng.gen_range(0..=text_rope.len());
                let start_ix = rng.gen_range(0..=end_ix);
                assert_eq!(
                    snapshot
                        .bytes_in_range(start_ix..end_ix)
                        .flatten()
                        .copied()
                        .collect::<Vec<_>>(),
                    expected_text.as_bytes()[start_ix..end_ix].to_vec(),
                    "bytes_in_range({:?})",
                    start_ix..end_ix,
                );
            }
        }

        let snapshot = multibuffer.read(cx).snapshot(cx);
        for (old_snapshot, subscription) in old_versions {
            let edits = subscription.consume().into_inner();

            log::info!(
                "applying subscription edits to old text: {:?}: {:?}",
                old_snapshot.text(),
                edits,
            );

            let mut text = old_snapshot.text();
            for edit in edits {
                let new_text: String = snapshot.text_for_range(edit.new.clone()).collect();
                text.replace_range(edit.new.start..edit.new.start + edit.old.len(), &new_text);
            }
            assert_eq!(text.to_string(), snapshot.text());
        }
    }

    #[gpui::test]
    fn test_history(cx: &mut MutableAppContext) {
        let buffer_1 = cx.add_model(|cx| Buffer::new(0, "1234", cx));
        let buffer_2 = cx.add_model(|cx| Buffer::new(0, "5678", cx));
        let multibuffer = cx.add_model(|_| MultiBuffer::new(0));
        let group_interval = multibuffer.read(cx).history.group_interval;
        multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.push_excerpt(
                ExcerptProperties {
                    buffer: &buffer_1,
                    range: 0..buffer_1.read(cx).len(),
                },
                cx,
            );
            multibuffer.push_excerpt(
                ExcerptProperties {
                    buffer: &buffer_2,
                    range: 0..buffer_2.read(cx).len(),
                },
                cx,
            );
        });

        let mut now = Instant::now();

        multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.start_transaction_at(now, cx);
            multibuffer.edit(
                [
                    Point::new(0, 0)..Point::new(0, 0),
                    Point::new(1, 0)..Point::new(1, 0),
                ],
                "A",
                cx,
            );
            multibuffer.edit(
                [
                    Point::new(0, 1)..Point::new(0, 1),
                    Point::new(1, 1)..Point::new(1, 1),
                ],
                "B",
                cx,
            );
            multibuffer.end_transaction_at(now, cx);
            assert_eq!(multibuffer.read(cx).text(), "AB1234\nAB5678");

            now += 2 * group_interval;
            multibuffer.start_transaction_at(now, cx);
            multibuffer.edit([2..2], "C", cx);
            multibuffer.end_transaction_at(now, cx);
            assert_eq!(multibuffer.read(cx).text(), "ABC1234\nAB5678");

            multibuffer.undo(cx);
            assert_eq!(multibuffer.read(cx).text(), "AB1234\nAB5678");

            multibuffer.undo(cx);
            assert_eq!(multibuffer.read(cx).text(), "1234\n5678");

            multibuffer.redo(cx);
            assert_eq!(multibuffer.read(cx).text(), "AB1234\nAB5678");

            multibuffer.redo(cx);
            assert_eq!(multibuffer.read(cx).text(), "ABC1234\nAB5678");

            buffer_1.update(cx, |buffer_1, cx| buffer_1.undo(cx));
            assert_eq!(multibuffer.read(cx).text(), "AB1234\nAB5678");

            multibuffer.undo(cx);
            assert_eq!(multibuffer.read(cx).text(), "1234\n5678");

            multibuffer.redo(cx);
            assert_eq!(multibuffer.read(cx).text(), "AB1234\nAB5678");

            multibuffer.redo(cx);
            assert_eq!(multibuffer.read(cx).text(), "ABC1234\nAB5678");

            multibuffer.undo(cx);
            assert_eq!(multibuffer.read(cx).text(), "AB1234\nAB5678");

            buffer_1.update(cx, |buffer_1, cx| buffer_1.redo(cx));
            assert_eq!(multibuffer.read(cx).text(), "ABC1234\nAB5678");

            multibuffer.undo(cx);
            assert_eq!(multibuffer.read(cx).text(), "C1234\n5678");
        });
    }
}
