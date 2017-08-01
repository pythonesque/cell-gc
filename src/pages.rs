//! Allocating pages of memory from the OS and carving them into individual
//! allocations. See TypedPage for details.

use heap::{GcHeap, HeapSessionId};
use marking::MarkingTracer;
use ptr::{Pointer, UntypedPointer};
use std::{cmp, mem, ptr};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use traits::{IntoHeapAllocation, Tracer};


/// Stores mark bits, pin counts, and an "am I in use?" bit for heap allocations.
struct MarkWord(usize);

const MARK_BIT: usize = 1;
const ALLOCATED_BIT: usize = 2;

/// Add the value `*p` to the root set, protecting it from GC.
///
/// A value that has been pinned *n* times stays in the root set
/// until it has been unpinned *n* times.
///
/// # Safety
///
/// `p` must point to a live allocation of type `T` in this heap.
pub unsafe fn pin<U>(p: Pointer<U>) {
    MarkWord::from_ptr(p, |mw| mw.pin());
}

/// Unpin a heap-allocated value (see `pin`).
///
/// # Safety
///
/// `p` must point to a pinned allocation of type `T` in this heap.
pub unsafe fn unpin<U>(p: Pointer<U>) {
    MarkWord::from_ptr(p, |mw| mw.unpin());
}

/// Unpin a heap allocation.
///
/// # Safety
///
/// `p` must point to a pinned allocation in this heap.
pub unsafe fn unpin_untyped(p: UntypedPointer) {
    MarkWord::from_untyped_ptr(p, |mw| mw.unpin());
}

pub unsafe fn get_mark_bit<U>(p: Pointer<U>) -> bool {
    MarkWord::from_ptr(p, |mw| mw.is_marked())
}

pub unsafe fn set_mark_bit<U>(p: Pointer<U>) {
    MarkWord::from_ptr(p, |mw| mw.mark());
}

const MARK_WORD_INIT: MarkWord = MarkWord(0);

impl MarkWord {
    unsafe fn from_ptr<U, F, R>(ptr: Pointer<U>, f: F) -> R
        where F: for<'a> FnOnce(&'a mut MarkWord) -> R
    {
        let addr = ptr.as_usize() - mem::size_of::<MarkWord>();
        f(&mut *(addr as *mut MarkWord))
    }

    unsafe fn from_untyped_ptr<F, R>(ptr: UntypedPointer, f: F) -> R
        where F: for<'a> FnOnce(&'a mut MarkWord) -> R
    {
        let addr = ptr.as_usize() - mem::size_of::<MarkWord>();
        f(&mut *(addr as *mut MarkWord))
    }

    fn is_allocated(&self) -> bool {
        self.0 & ALLOCATED_BIT != 0
    }

    fn set_allocated(&mut self) {
        self.0 |= ALLOCATED_BIT;
    }

    fn clear_allocated(&mut self) {
        self.0 &= !ALLOCATED_BIT;
    }

    fn is_marked(&self) -> bool {
        self.0 & MARK_BIT != 0
    }

    fn mark(&mut self) {
        self.0 |= MARK_BIT;
    }

    fn unmark(&mut self) {
        self.0 &= !MARK_BIT;
    }

    fn is_pinned(&self) -> bool {
        self.0 >> 2 != 0
    }

    fn pin(&mut self) {
        debug_assert!(self.is_allocated());
        self.0 += 4;
    }

    fn unpin(&mut self) {
        debug_assert!(self.is_allocated());
        debug_assert!(self.is_pinned());
        self.0 -= 4;
    }
}

/// Non-inlined function that serves as an entry point to marking. This is used
/// for marking root set entries.
unsafe fn mark_entry_point<'h, T>(addr: UntypedPointer, tracer: &mut MarkingTracer)
where
    T: IntoHeapAllocation<'h>,
{
    let addr = addr.as_typed_ptr::<T::In>();

    if get_mark_bit(addr) {
        // If the mark bit is set, then this object is gray in the classic
        // tri-color sense: seen but we just popped it off the mark stack and
        // have't finished enumerating its outgoing edges.
        T::trace(addr.as_ref(), tracer);
    } else {
        // If the mark bit is not set, then this object is white in the classic
        // tri-color sense: freshly discovered to be live, and we now need to
        // set its mark bit and then process its edges or push it onto the mark
        // stack for later edge processing.
        tracer.visit::<T>(addr);
    }
}

/// A unique id for each type that implements `IntoHeapAllocation`.
///
/// Implementation note: function types don't support Eq, so we cast to a
/// meaningless pointer type.
#[derive(Debug, Hash, PartialEq, Eq)]
pub struct TypeId(*const ());

pub fn heap_type_id<'h, T: IntoHeapAllocation<'h>>() -> TypeId {
    TypeId(mark_entry_point::<T> as *const ())
}

pub(crate) const PAGE_SIZE: usize = 0x1000;

/// We rely on all bits to the right of this bit being 0 in addresses of
/// TypedPage instances.
pub(crate) const PAGE_ALIGN: usize = 0x1000;

fn is_aligned(ptr: *const ()) -> bool {
    ptr as usize & (PAGE_ALIGN - 1) == 0
}

pub struct PageHeader {
    pub heap: *mut GcHeap,
    next_page: *mut PageHeader,
    mark_fn: unsafe fn(UntypedPointer, &mut MarkingTracer),
    freelist: *mut (),
    allocation_size: usize,
}

impl PageHeader {
    pub fn find(ptr: UntypedPointer) -> *mut PageHeader {
        let header_addr = ptr.as_usize() & !(PAGE_ALIGN - 1);
        assert!(header_addr != 0);
        header_addr as *mut PageHeader
    }

    pub unsafe fn mark(&self, ptr: UntypedPointer, tracer: &mut MarkingTracer) {
        (self.mark_fn)(ptr, tracer);
    }

    pub fn type_id(&self) -> TypeId {
        TypeId(self.mark_fn as *const ())
    }

    pub fn downcast_mut<'h, T>(&mut self) -> Option<&mut TypedPage<T::In>>
    where
        T: IntoHeapAllocation<'h>,
    {
        if heap_type_id::<T>() == self.type_id() {
            let ptr = self as *mut PageHeader as *mut TypedPage<T::In>;
            Some(unsafe { &mut *ptr })
        } else {
            None
        }
    }

    fn begin_offset() -> usize {
        mem::size_of::<PageHeader>()
    }

    /// Address of the first allocation on this page.
    fn begin(&self) -> usize {
        (self as *const PageHeader as usize) + Self::begin_offset()
    }

    fn end(&self) -> usize {
        let capacity = (PAGE_SIZE - Self::begin_offset()) / self.allocation_size;
        self.begin() + capacity * self.allocation_size
    }

    pub fn clear_mark_bits(&mut self, roots: &mut Vec<UntypedPointer>) {
        let mut addr = self.begin();
        let end = self.end();
        while addr < end {
            let mark_word = unsafe { &mut *(addr as *mut MarkWord) };
            mark_word.unmark();
            if mark_word.is_pinned() {
                let ptr =
                    unsafe {
                        UntypedPointer::new((addr + mem::size_of::<MarkWord>()) as *const ())
                    };
                roots.push(ptr);
            }
            addr += self.allocation_size;
        }
    }

    /// True if nothing on this page is allocated.
    pub fn is_empty(&self) -> bool {
        let mut addr = self.begin();
        let end = self.end();
        while addr < end {
            let mark_word = unsafe { &mut *(addr as *mut MarkWord) };
            if mark_word.is_allocated() {
                return false;
            }
            addr += self.allocation_size;
        }
        true
    }
}

/// A page of memory where heap-allocated objects of a particular type are stored.
///
/// A GcHeap is a collection of PageSets, and each PageSet is a collection of
/// TypedPages.
///
/// The layout of a page is like this:
///
/// ```ignore
/// struct TypedPage<U> {
///     header: PageHeader
///     allocations: [Allocation<U>; Self::capacity()]
/// }
///
/// struct Allocation<U> {
///     mark_word: MarkWord,
///     union {
///         value: U,
///         free_list_chain: *mut U,
///     }
/// }
/// ```
///
/// where `Self::capacity()` is computed so as to make all this fit in 4KB.
/// (Allocations larger than 4KB are not supported.)
///
/// Since Rust doesn't support that particular kind of union yet, we implement
/// this data structure with pointer arithmetic and hackery.
///
/// The `(MarkWord, U)` pairs in the page are called "allocations".  Each
/// allocation is either in use (containing an actual value of type `U`) or
/// free (uninitialized memory). The `MarkWord` distinguishes these two cases.
/// All free allocations are in the freelist.
///
/// In addition, an allocation that's in use can be "pinned", making it part of
/// the root set. GcRefs outside the heap keep their referents pinned.
///
/// Trivia: This wastes a word when size_of<U>() is 0; the MarkWord (rather
/// than the value field) could contain the free-list chain. However, the
/// direction we'd like to go is to get rid of pin counts.
pub struct TypedPage<U> {
    pub header: PageHeader,
    pub allocations: PhantomData<U>,
}

impl<U> Deref for TypedPage<U> {
    type Target = PageHeader;

    fn deref(&self) -> &PageHeader {
        &self.header
    }
}

impl<U> DerefMut for TypedPage<U> {
    fn deref_mut(&mut self) -> &mut PageHeader {
        &mut self.header
    }
}

/// Returns the smallest multiple of `k` that is at least `n`.
///
/// Panics if the answer is too big to fit in a `usize`.
fn round_up(n: usize, k: usize) -> usize {
    let a = n / k * k;
    if a == n {
        n
    } else {
        n + k
    }
}

impl<U> TypedPage<U> {
    /// The actual size of an allocation can't be smaller than the size of a
    /// pointer, due to the way we store the freelist by stealing a pointer
    /// from the allocation itself.
    fn allocation_size() -> usize {
        mem::size_of::<MarkWord>() + round_up(cmp::max(mem::size_of::<U>(), mem::size_of::<*mut U>()),
                                              mem::align_of::<MarkWord>())
    }

    /// Offset, in bytes, of the first allocation from the start of the page.
    pub(crate) fn first_allocation_offset() -> usize {
        mem::size_of::<PageHeader>()
    }

    /// Number of allocations that fit in a page.
    pub fn capacity() -> usize {
        (PAGE_SIZE - Self::first_allocation_offset()) / Self::allocation_size()
    }

    /// Address of the first allocation in this page.
    fn begin(&self) -> usize {
        (self as *const Self as usize) + Self::first_allocation_offset()
    }

    /// Address one past the end of this page's array of allocations.
    fn end(&self) -> usize {
        // Everything after the first plus sign here is a constant expression.
        //
        // Addition will overflow if `self` is literally the last page in
        // virtual memory—which can't happen—and the constant works out to
        // PAGE_SIZE, which can.
        (self as *const Self as usize) + (Self::first_allocation_offset() +
                                          Self::capacity() * Self::allocation_size())
    }

    unsafe fn init_mark_words_and_freelist(&mut self) {
        let mut addr = self.begin();
        let end = self.end();
        while addr < end {
            let mark_word = addr as *mut MarkWord;
            ptr::write(mark_word, MARK_WORD_INIT);
            self.add_to_free_list((addr + mem::size_of::<MarkWord>()) as *mut U);

            // This can't use `ptr = ptr.offset(1)` because if U is smaller
            // than a pointer, allocations are padded to pointer size.
            // `.offset(1)` doesn't know about the padding and therefore
            // wouldn't advance to the next allocation.
            addr += Self::allocation_size();
        }
    }

    /// Return the page containing the object `ptr` points to.
    pub fn find(ptr: Pointer<U>) -> *mut TypedPage<U> {
        PageHeader::find(ptr.into()) as *mut TypedPage<U>
    }

    unsafe fn add_to_free_list(&mut self, p: *mut U) {
        let listp = p as *mut *mut ();
        *listp = self.header.freelist;
        assert_eq!(*listp, self.header.freelist);
        self.header.freelist = p as *mut ();
    }

    /// Allocate a `U`-sized-and-aligned region of uninitialized memory
    /// from this page.
    ///
    /// # Safety
    ///
    /// This is safe unless GC is happening.
    pub unsafe fn try_alloc(&mut self) -> Option<Pointer<U>> {
        let p = self.header.freelist;
        if p.is_null() {
            None
        } else {
            let listp = p as *mut *mut ();
            self.header.freelist = *listp;
            let ap = Pointer::new(p as *mut U);
            MarkWord::from_ptr(ap, |mw| {
                assert!(!mw.is_allocated());
                mw.set_allocated();
            });
            Some(ap)
        }
    }

    unsafe fn sweep(&mut self) -> bool {
        let mut addr = self.begin();
        let end = self.end();
        let mut swept_any = false;
        while addr < end {
            let mw = &mut *(addr as *mut MarkWord);
            if mw.is_allocated() && !mw.is_marked() {
                let object_ptr = (addr + mem::size_of::<MarkWord>()) as *mut U;
                ptr::drop_in_place(object_ptr);
                if cfg!(debug_assertions) || cfg!(test) {
                    // Paint the unused memory with a known-bad value.
                    const SWEPT_BYTE: u8 = 0xf4;
                    ptr::write_bytes(object_ptr, SWEPT_BYTE, 1);
                }
                mw.clear_allocated();
                self.add_to_free_list(object_ptr);
                swept_any = true;
            }
            addr += Self::allocation_size();
        }

        swept_any
    }
}

/// Sweep a page.
///
/// # Safety
///
/// This must be called only after a full mark phase, to avoid sweeping objects
/// that are still reachable.
unsafe fn sweep_entry_point<'h, T: IntoHeapAllocation<'h>>(header: &mut PageHeader) -> bool {
    header.downcast_mut::<T>().expect("page header corrupted").sweep()
}

/// An unordered collection of memory pages that all share an allocation type.
///
/// All pages in this collection have matching `.heap` and `.mark_fn` fields.
pub struct PageSet {
    heap: *mut GcHeap,

    sweep_fn: unsafe fn(&mut PageHeader) -> bool,

    /// Total number of pages in the following lists.
    page_count: usize,

    /// Head of the linked list of fully allocated pages.
    full_pages: *mut PageHeader,

    /// Head of the linkedlist of nonfull pages.
    other_pages: *mut PageHeader,

    /// The maximum number of pages, or None for no limit.
    limit: Option<usize>,
}

/// Apply a closure to every page in a linked list.
fn each_page<F: FnMut(&PageHeader)>(first_page: *mut PageHeader, mut f: F) {
    unsafe {
        let mut page = first_page;
        while !page.is_null() {
            let header = &*page;
            f(header);
            page = header.next_page;
        }
    }
}

/// Apply a closure to every page in a linked list.
fn each_page_mut<F: FnMut(&mut PageHeader)>(first_page: *mut PageHeader, mut f: F) {
    unsafe {
        let mut page = first_page;
        while !page.is_null() {
            let header = &mut *page;
            f(header);
            page = header.next_page;
        }
    }
}

impl Drop for PageSet {
    fn drop(&mut self) {
        // Don't use each_page here: we're dropping them.
        for page_list in &[self.full_pages, self.other_pages] {
            let mut page = *page_list;
            while !page.is_null() {
                unsafe {
                    let mut roots_to_ignore = vec![];
                    let next = (*page).next_page;
                    (*page).clear_mark_bits(&mut roots_to_ignore);
                    (self.sweep_fn)(&mut *page); // drop all objects remaining in the page
                    ptr::drop_in_place(page); // drop the header
                    Vec::from_raw_parts(page as *mut u8, 0, PAGE_SIZE); // free the page
                    page = next;
                }
            }
        }
    }
}

impl PageSet {
    /// Create a new PageSet.
    ///
    /// # Safety
    ///
    /// Safe as long as `heap` is a valid pointer.
    pub unsafe fn new<'h, T: IntoHeapAllocation<'h>>(heap: *mut GcHeap) -> PageSet {
        PageSet {
            heap,
            sweep_fn: sweep_entry_point::<T>,
            page_count: 0,
            full_pages: ptr::null_mut(),
            other_pages: ptr::null_mut(),
            limit: None,
        }
    }

    /// Downcast to a typed PageSetRef.
    ///
    /// # Panics
    ///
    /// If T is not the actual allocation type for this page set.
    pub fn downcast_mut<'a, 'h, T>(&'a mut self) -> PageSetRef<'a, 'h, T>
    where
        T: IntoHeapAllocation<'h> + 'a,
    {
        assert_eq!(
            self.sweep_fn as *const (),
            sweep_entry_point::<T> as *const ()
        );

        PageSetRef {
            page_set: self,
            id: PhantomData,
            also: PhantomData,
        }
    }

    fn each_page<F: FnMut(&PageHeader)>(&self, mut f: F) {
        each_page(self.full_pages, &mut f);
        each_page(self.other_pages, &mut f);
    }

    fn each_page_mut<F: FnMut(&mut PageHeader)>(&mut self, mut f: F) {
        each_page_mut(self.full_pages, &mut f);
        each_page_mut(self.other_pages, &mut f);
    }

    /// Clear mark bits from each page in this set.
    ///
    /// # Safety
    ///
    /// This must be called only at the beginning of a GC cycle.
    pub unsafe fn clear_mark_bits(&mut self, roots: &mut Vec<UntypedPointer>) {
        self.each_page_mut(|page| page.clear_mark_bits(roots));
    }

    /// Sweep all unmarked objects from all pages.
    ///
    /// # Safety
    ///
    /// Safe to call only as the final part of GC.
    pub unsafe fn sweep(&mut self) {
        // Sweep nonfull pages.
        each_page_mut(self.other_pages, |page| {
            (self.sweep_fn)(page);
        });

        // Sweep full pages. Much more complicated because we have to move
        // pages from one list to the other if any space is freed.
        let mut prev_page = &mut self.full_pages;
        let mut page = *prev_page;
        while !page.is_null() {
            if (self.sweep_fn)(&mut *page) {
                let next_page = (*page).next_page;

                // remove from full list
                *prev_page = next_page;

                // add to nonfull list
                (*page).next_page = self.other_pages;
                self.other_pages = page;

                page = next_page;
            } else {
                prev_page = &mut (*page).next_page;
                page = *prev_page;
            }
        }
    }

    /// True if nothing is allocated in this set of pages.
    pub fn all_pages_are_empty(&self) -> bool {
        let mut empty = true;
        self.each_page(|page| { empty &= page.is_empty(); });
        empty
    }

    pub fn set_page_limit(&mut self, limit: Option<usize>) {
        self.limit = limit;
    }
}

pub struct PageSetRef<'a, 'h, T: IntoHeapAllocation<'h> + 'a> {
    page_set: &'a mut PageSet,
    id: HeapSessionId<'h>,
    also: PhantomData<&'a mut T>,
}

impl<'a, 'h, T: IntoHeapAllocation<'h> + 'a> Deref for PageSetRef<'a, 'h, T> {
    type Target = PageSet;

    fn deref(&self) -> &PageSet { self.page_set }
}

impl<'a, 'h, T: IntoHeapAllocation<'h> + 'a> DerefMut for PageSetRef<'a, 'h, T> {
    fn deref_mut(&mut self) -> &mut PageSet { self.page_set }
}

impl<'a, 'h, T: IntoHeapAllocation<'h> + 'a> PageSetRef<'a, 'h, T> {
    /// Allocate memory for a value of type `T::In`.
    ///
    /// # Safety
    ///
    /// Safe to call as long as GC is not happening.
    pub unsafe fn try_alloc(&mut self) -> Option<Pointer<T::In>> {
        // First, try to allocate from an existing page.
        let front_page = self.other_pages;
        if !front_page.is_null() {
            // We have a nonfull page. Allocation can't fail.
            assert!(!(*front_page).freelist.is_null());
            let page = (*front_page).downcast_mut::<T>().unwrap();
            let ptr = page.try_alloc().unwrap();

            // If the page is full now, move it to the other list.
            if page.freelist.is_null() {
                // Pop this page from the nonfull page list.
                self.other_pages = page.next_page;

                // Add it to the full-page list.
                page.next_page = self.full_pages;
                self.full_pages = &mut page.header;
            }
            return Some(ptr);
        }

        // If there is a limit and we already have at least that many pages, fail.
        match self.limit {
            Some(limit) if self.page_count >= limit => None,
            _ => self.new_page().try_alloc(),
        }
    }

    /// Allocate a page from the operating system.
    ///
    /// Initialize its header and freelist and link it into this page set's
    /// linked list of pages.
    fn new_page(&mut self) -> &mut TypedPage<T::In> {
        let capacity = TypedPage::<T::In>::capacity();
        assert!({
            let size_of_page = mem::size_of::<TypedPage<T::In>>();
            let alloc_offset = TypedPage::<T::In>::first_allocation_offset();
            size_of_page <= alloc_offset
        });
        assert!({
            let alloc_offset = TypedPage::<T::In>::first_allocation_offset();
            let alloc_size = TypedPage::<T::In>::allocation_size();
            alloc_offset + capacity * alloc_size <= PAGE_SIZE
        });

        // All allocations in a page are pointer-size-aligned. If this isn't
        // good enough for T::In, panic.
        {
            let word_size = mem::size_of::<usize>();
            assert_eq!(mem::size_of::<MarkWord>(), word_size);
            assert!(mem::align_of::<T::In>() <= word_size,
                    "Types with exotic alignment requirements are not supported");
        }

        let mut vec: Vec<u8> = Vec::with_capacity(PAGE_SIZE);
        let raw_page = vec.as_mut_ptr() as *mut ();

        // Rust makes no guarantee whatsoever that this will work.
        // If it doesn't, panic.
        assert!(is_aligned(raw_page));

        let page_ptr: *mut TypedPage<T::In> = raw_page as *mut TypedPage<T::In>;
        unsafe {
            // Normally we insert the new page in the nonfull page list.
            // However, if T::In is so large that only one allocation fits in a
            // page, the new page must go directly into the full page list.
            let list_head =
                if capacity == 1 {
                    &mut self.page_set.full_pages
                } else {
                    &mut self.page_set.other_pages
                };

            ptr::write(
                page_ptr,
                TypedPage {
                    header: PageHeader {
                        heap: self.page_set.heap,
                        next_page: *list_head,
                        mark_fn: mark_entry_point::<T>,
                        freelist: ptr::null_mut(),
                        allocation_size: TypedPage::<T::In>::allocation_size()
                    },
                    allocations: PhantomData,
                },
            );

            let page = &mut *page_ptr;
            page.init_mark_words_and_freelist();

            // Remove the memory from the vector and link it into
            // the PageSet's linked list.
            mem::forget(vec);
            *list_head = &mut page.header;
            self.page_set.page_count += 1;

            page
        }
    }
}
