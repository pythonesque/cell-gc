//! Check that Rust does not let us drop the heap while references into it
//! still exist (they would be dangling pointers). The first iteration of
//! `with_heap` passed the heap to the callback by value, and thus was
//! susceptible to this bug! It caused:
//!
//!     thread panicked while panicking. aborting.
//!
//! The fix was to pass the heap to the closure by reference. (The API has
//! changed since then; now we have heap sessions and this is less of a
//! potential pitfall.)

extern crate cell_gc;
#[macro_use] extern crate cell_gc_derive;
mod pairs_aux;
use cell_gc::*;
use pairs_aux::*;

fn main() {
    with_heap(|hs| {
        let obj = alloc_null_pair(hs);

        // note: dropping `hs` would just drop the reference, which is no problem
        std::mem::drop(*hs);
        //~^ ERROR cannot move out of borrowed content

        let val = Value::Pair(obj.clone());
        obj.set_head(val); // occurs after the heap session is gone
    });
}
