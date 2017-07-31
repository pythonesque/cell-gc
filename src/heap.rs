//! GC heap allocator.
//!
//! ### Safety
//!
//! First, a few general observations about safety in Rust.
//!
//! *   If a `mut` reference to a value exists, and other references to the value
//!     also exist, we can definitely crash.
//!
//!     One way it can happen is that the `mut` value is or contains an
//!     `enum`. Using the other reference, we can borrow a refrence to a
//!     current field of the `enum`. Then, using the `mut` reference, we can
//!     assign some other variant to the `enum`, invalidating the reference.
//!     Rust does not detect that the reference is invalid, so when we use it,
//!     we are accessing gibberish.
//!
//!     Another way is if a callback is called while we're in the middle of
//!     mutating a value, while it's in an invalid state. When you read
//!     "callback", think of `deref` and `drop` methods, which are called
//!     implicitly all over the place. These callbacks are normally safe,
//!     because they simply can't have a reference to the value that's in an
//!     intermediate state. But if another reference exists, it might be used
//!     to try to read from the half-mutated value.
//!
//! *   If a data structure contains two paths to a value, under Rust's usual
//!     rules, you can get two `mut` references to that value.
//!
//!     This is why you tend to build ownership trees in Rust and it's why GC
//!     is particularly challenging in Rust: GCs build an arbitrary graph of
//!     values and references.
//!
//! This GC takes the following approach to ensure safety.
//!
//! *   Minimize access to values stored in the heap. For the most part,
//!     application code *never* sees a direct reference to any value that is
//!     physically stored someplace where it's subject to GC.
//!
//! *   Minimize the times when direct references to in-heap values exist at
//!     all, and during these operations, prevent control from escaping to
//!     arbitrary application code.
//!
//! *   Ensure that when any direct references to in-heap values exist, they
//!     obey Rust's rules: for any given value, either only non-`mut`
//!     references, or at most one `mut` reference, exists at a time.
//!
//! Thus we are particularly keen to avoid the possibility of "reentering" the
//! heap, creating new references to in-heap values while others already exist.
//!
//! References to heap values therefore exist only during the following
//! operations:
//!
//! *   Allocation - That is, moving values into the heap. This is safe because
//!     it never triggers any user code at all while heap references exist.
//!
//! * - Heap reads and writes - The only way to do these is via macro-generated
//!     accessors which do not expose references.  Reads call `from_heap()` on
//!     in-heap values, which is dangerous because `from_heap()` receives a
//!     direct reference.  Writes call `drop()`, which is even more dangerous:
//!     (1) it receives a direct `mut` reference; and (2) it leaves in-heap
//!     values uninitialized.
//!
//! *   GC marking - The code for this is all macro-generated.
//!
//! *   GC sweeping - This calls `drop()`, which is dangerous for the reasons
//!     noted above.
//!
//! To make this scheme safe, `from_heap()` and `drop()` must be tightly controlled.
//! `from_heap()` is therefore in an unsafe trait; users are expected to use
//! `#[derive(IntoHeap)]` to autogenerate instances.
//!
//! However, **we leave it up to the user to exercise care with `drop()`.**
//! We suggest *never* implementing `Drop` for a heap type. If you must,
//! avoid reading pointer fields while dropping, and avoid calling into
//! arbitrary code.

use gc_ref::{GcFrozenRef, GcRef};
use marking::{MarkingTracer, mark};
use pages::{self, PageSet, PageSetRef, TypeId, TypedPage, heap_type_id};
use ptr::{Pointer, UntypedPointer};
use signposts;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::{Arc, Mutex, Weak};
use traits::IntoHeapAllocation;

/// A universe in which you can store values that implement
/// `IntoHeapAllocation`. The values are mutable and they can point to each
/// other, in cycles.
pub struct GcHeap {
    /// Map from heap types to the set of pages for that type.
    page_sets: HashMap<TypeId, PageSet>,

    /// Tracer for the mark phase of GC.
    marking_tracer: Option<MarkingTracer>,

    /// List of pointers that should be unpinned before the next GC cycle. The
    /// `GcFrozenRef` destructor uses this to unpin pointers even though frozen
    /// refs can be sent across thread boundaries.
    ///
    /// The address of this `Mutex` also serves as a unique id for this heap.
    /// `GcFrozenRef` uses it to prevent you from freezing a reference into
    /// one heap, then thawing it in a different heap, you monster.
    dropped_frozen_ptrs: Arc<Mutex<Vec<UntypedPointer>>>,
}

unsafe impl Send for GcHeap {}

/// An opaque unique id for heaps.
#[derive(Clone)]
pub struct HeapId(Weak<Mutex<Vec<UntypedPointer>>>);

// What does this do? You'll never guess!
pub type HeapSessionId<'h> = PhantomData<::std::cell::Cell<&'h mut ()>>;

pub struct GcHeapSession<'h> {
    id: HeapSessionId<'h>,

    /// The heap. It's important that this is an exclusive reference and is
    /// *not* exposed to other code. If other code could call heap.enter() and
    /// create another session at the same time, we could crash.
    heap: &'h mut GcHeap,
}

/// Create a heap, pass it to a callback, then destroy the heap.
///
/// The heap's lifetime is directly tied to this function call, for safety. (So
/// the API is a little wonky --- but how many heaps were you planning on
/// creating?)
pub fn with_heap<R, F>(f: F) -> R
where
    F: for<'h> FnOnce(&mut GcHeapSession<'h>) -> R,
{
    GcHeap::new().enter(f)
}

impl GcHeap {
    /// Create a new, empty heap.
    pub fn new() -> GcHeap {
        GcHeap {
            page_sets: HashMap::new(),
            marking_tracer: Some(MarkingTracer::default()),
            dropped_frozen_ptrs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Get this heap's unique id.
    fn id(&self) -> HeapId {
        HeapId(Arc::downgrade(&self.dropped_frozen_ptrs))
    }

    /// Panic if `heap_id` isn't this heap's id.
    fn check_id(&self, heap_id: HeapId) {
        // Panic if thawing across heaps.
        let heap_id_arc = heap_id
            .0
            .upgrade()
            .expect("can't thaw a reference into a heap that has been dropped");
        assert!(
            Arc::ptr_eq(&heap_id_arc, &self.dropped_frozen_ptrs),
            "can't thaw a frozen reference into a different heap"
        );
    }

    pub(crate) fn drop_frozen_ptr(heap_id: HeapId, ptr: UntypedPointer) {
        // If the heap still exists, add ptr to its internal list of dropped
        // pointers. If not, do nothing; the value was already unpinned and
        // dropped when the heap was dropped.
        if let Some(heap_id_arc) = heap_id.0.upgrade() {
            let mut guard = heap_id_arc.lock().unwrap();
            guard.push(ptr);
        }
    }

    /// Start a session to access this heap.
    ///
    /// You need a `GcHeapSession` in order to do anything interesting with a
    /// heap.  Each heap has either 0 or 1 `GcHeapSession` at a time, and a
    /// `GcHeapSession` is bound to a stack lifetime, so Rust can enforce
    /// safety rules.
    ///
    /// It would be safe to make this method public, but it's a pain in
    /// practice. You'll want to pass a `&mut GcHeapSession<'h>` around, not a
    /// `GcHeapSession<'h>`, since `GcHeapSession` is not `Copy`. Use `enter`.
    fn open<'h>(&'h mut self) -> GcHeapSession<'h> {
        GcHeapSession {
            id: PhantomData,
            heap: self,
        }
    }

    /// Run some code using this GcHeap.
    ///
    /// # Example
    ///
    ///     use cell_gc::{GcHeap, GcLeaf};
    ///
    ///     let mut heap = GcHeap::new();
    ///     heap.enter(|hs| {
    ///         // ... hs.alloc(MyHeapStruct { ... }) ...
    ///         # hs.force_gc();
    ///     });
    ///
    pub fn enter<R, F>(&mut self, f: F) -> R
    where
        F: for<'h> FnOnce(&mut GcHeapSession<'h>) -> R,
    {
        f(&mut self.open())
    }

    pub unsafe fn from_allocation<'h, T: IntoHeapAllocation<'h>>(
        ptr: Pointer<T::In>,
    ) -> *const GcHeap {
        (*TypedPage::find(ptr)).header.heap
    }

    fn take_marking_tracer(&mut self) -> MarkingTracer {
        self.marking_tracer.take().expect("attempted nested GC")
    }

    fn replace_marking_tracer(&mut self, tracer: MarkingTracer) {
        assert!(self.marking_tracer.is_none());
        assert!(tracer.mark_stack_is_empty());
        self.marking_tracer = Some(tracer);
    }

    /// Run the given function with the marking tracer.
    ///
    /// The marking tracer is taken out of the heap and replaced again so we can
    /// have two independent borrows of the heap and the marking tracer and the
    /// same time.
    pub(crate) fn with_marking_tracer<F, O>(&mut self, mut f: F) -> O
    where
        F: FnMut(&mut Self, &mut MarkingTracer) -> O,
    {
        let mut tracer = self.take_marking_tracer();
        let retval = f(self, &mut tracer);
        self.replace_marking_tracer(tracer);
        retval
    }

    /// Clear all mark bits in preparation for GC.
    ///
    /// # Safety
    ///
    /// This must be called only at the beginning of a GC cycle.
    pub(crate) unsafe fn clear_mark_bits(&mut self, roots: &mut Vec<UntypedPointer>) {
        for page_set in self.page_sets.values_mut() {
            page_set.clear_mark_bits(roots);
        }
    }

    fn unpin_dropped_ptrs(&mut self) {
        let dropped_ptrs = {
            let mut guard = self.dropped_frozen_ptrs.lock().unwrap();
            let main: &mut Vec<UntypedPointer> = &mut guard;
            let mut tmp: Vec<UntypedPointer> = Vec::new();
            mem::swap(main, &mut tmp);
            tmp
        };

        for p in dropped_ptrs {
            unsafe {
                pages::unpin_untyped(p);
            }
        }
    }

    /// Perform GC. This is called from `<GcHeap as Drop>::drop()`, so
    /// unreachable values found by GC must be dropped synchronously, before
    /// this returns.
    fn gc(&mut self) {
        self.gc_cycle(false);
    }

    fn gc_cycle(&mut self, dropping: bool) {
        self.unpin_dropped_ptrs();
        mark(self, dropping);

        let _sp = signposts::Sweeping::new();
        for page_set in self.page_sets.values_mut() {
            unsafe {
                page_set.sweep();
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.page_sets
            .values()
            .all(|page_set| page_set.all_pages_are_empty())
    }
}

impl Drop for GcHeap {
    fn drop(&mut self) {
        // Perform a final GC to call destructors on any remaining allocations.
        // We do not mark anything. This is safe because nothing that's pinned
        // will ever be touched again; allocations can be pinned when we get
        // here, but only if we leaked a `GcRef`.
        self.gc_cycle(true);

        for page_set in self.page_sets.values() {
            assert!(page_set.all_pages_are_empty());
        }
    }
}

impl<'h> GcHeapSession<'h> {
    fn get_page_set<'a, T: IntoHeapAllocation<'h> + 'a>(&'a mut self) -> PageSetRef<'a, 'h, T> {
        let key = heap_type_id::<T>();
        let heap: *mut GcHeap = self.heap;
        self.heap
            .page_sets
            .entry(key)
            .or_insert_with(|| unsafe { PageSet::new::<T>(heap) })
            .downcast_mut()
    }

    /// Set (or unset) the limit on the number of pages that can be used to
    /// allocate values of type `T` in this heap. By default, no limit is set.
    ///
    /// See `try_alloc` for more.
    ///
    /// If there are already at least `limit` pages for `T` values, this may have no effect;
    /// it doesn't cause pages to be freed.
    pub fn set_page_limit<T: IntoHeapAllocation<'h>>(&mut self, limit: Option<usize>) {
        self.get_page_set::<T>().set_page_limit(limit);
    }

    /// Allocate memory, moving `value` into the heap.
    ///
    /// If a limit has previously been set using `set_page_limit`, and we run
    /// up against the limit (already have at least that many pages for `T`
    /// values, and they are all full of live values), `try_alloc` first
    /// attempts to free some memory by doing garbage collection. If that
    /// doesn't work, `try_alloc` returns `None`.
    pub fn try_alloc<T: IntoHeapAllocation<'h>>(&mut self, value: T) -> Option<T::Ref> {
        // For now, this is done very early, so that if it panics, the heap is
        // left in an OK state. Better wrapping of raw pointers would make it
        // possible to do this later, closer to the `ptr::write()` call. (And
        // the compiler might optimize away this temporary if we do it that
        // way.) Looking forward to placement new!
        let mut u = value.into_heap();
        unsafe {
            let p = match self.get_page_set::<T>().try_alloc() {
                Some(p) => p,
                None => {
                    // Careful: moving `value` into the heap may have unpinned
                    // other objects it points to. Re-pin them before
                    // attempting GC! And rebuild `u` afterward, since the GC
                    // doesn't know it exists.
                    let tmp_value = T::from_heap(&u);
                    drop(u);
                    self.heap.gc();
                    u = tmp_value.into_heap();
                    match self.get_page_set::<T>().try_alloc() {
                        Some(p) => p,
                        None => return None,
                    }
                }
            };

            ptr::write(p.as_raw() as *mut _, u);
            let gc_ref = T::wrap_gc_ref(GcRef::new(p));
            Some(gc_ref)
        }
    }

    /// Allocate memory, moving `T` into the heap. This may cause garbage collection.
    ///
    /// # Panics
    ///
    /// If a page limit has been set, all pages are full, and GC fails to shake
    /// anything loose.
    pub fn alloc<T: IntoHeapAllocation<'h>>(&mut self, value: T) -> T::Ref {
        let _sp = signposts::Allocating::new();
        self.try_alloc(value)
            .expect("out of memory (gc did not collect anything)")
    }

    /// Do garbage collection.
    pub fn force_gc(&mut self) {
        self.heap.gc();
    }

    pub fn freeze<T: IntoHeapAllocation<'h>>(&self, t: T::Ref) -> GcFrozenRef<T> {
        GcFrozenRef::new(&self, t)
    }

    pub fn thaw<T: IntoHeapAllocation<'h>>(&self, t: GcFrozenRef<T>) -> T::Ref {
        T::wrap_gc_ref(t.thaw(&self))
    }

    pub(crate) fn heap_id(&self) -> HeapId {
        self.heap.id()
    }

    pub(crate) fn check_heap_id(&self, heap_id: HeapId) {
        self.heap.check_id(heap_id);
    }

    /// Returns true if all allocations have been collected. This implies that
    /// no `GcRef`s into the heap exist. You may need to call `hs.force_gc()`
    /// before this, to get predictable results.
    ///
    /// This method is provided for testing only and may disappear without
    /// warning.
    #[doc(hidden)]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}
