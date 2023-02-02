use std::{cmp::min, slice};

use crate::{
    bindings::{
        composition_hash_table,
        composition_method,
        glyph,
        glyph_string,
        XHASH_TABLE,
        composition_gstring_from_id,
        AREF,
        lglyph_indices::{LGLYPH_IX_ADJUSTMENT,
                         LGLYPH_IX_WIDTH},
        VECTORP},
    definitions::EmacsInt,
    lisp::{ExternalPtr, LispObject},
};

pub type XChar2b = u32;

pub type GlyphRef = ExternalPtr<glyph>;
pub type GlyphStringRef = ExternalPtr<glyph_string>;

impl GlyphStringRef {
    pub fn get_chars(&self) -> &[XChar2b] {
        let len = self.nchars as usize;

        unsafe { slice::from_raw_parts(self.char2b, len) }
    }

    pub fn first_glyph(&self) -> GlyphRef {
        self.first_glyph.into()
    }

    pub fn composite_offsets(&self) -> &[i16] {
        let len = (self.nchars * 2) as usize;

        let offsets = unsafe { slice::from_raw_parts((*self.cmp).offsets, len) };

        let from = (self.cmp_from * 2) as usize;
        let to = min((self.cmp_to * 2) as usize, len);

        &offsets[from..to]
    }

    pub fn composite_chars(&self) -> &[XChar2b] {
        let from = self.cmp_from as usize;
        let to = min(self.cmp_to, self.nchars) as usize;

        &self.get_chars()[from..to]
    }

    pub fn composite_glyph(&self, n: usize) -> EmacsInt {
        let n = self.cmp_from as usize + n;

        let hash_table = unsafe { XHASH_TABLE(composition_hash_table) };

        let key_and_value = unsafe { (*hash_table).key_and_value }.as_vector().unwrap();

        let composition_index = (unsafe { (*self.cmp).hash_index } * 2) as usize;
        let composition =
            unsafe { key_and_value.contents.as_slice(composition_index + 1) }[composition_index];
        let composition = composition.as_vector().unwrap();

        let glyph_index = if unsafe { (*self.cmp).method }
        == composition_method::COMPOSITION_WITH_RULE_ALTCHARS
        {
            n * 2
        } else {
            n
        };

        let glyph = unsafe { composition.contents.as_slice(glyph_index + 1) }[glyph_index];

        glyph.as_fixnum_or_error()
    }


}

impl IntoIterator for GlyphStringRef {
    type Item = GlyphStringRef;
    type IntoIter = GlyphStringIntoIterator;

    fn into_iter(self) -> Self::IntoIter {
        GlyphStringIntoIterator {
            next_glyph_string: Some(self),
        }
    }
}

pub struct GlyphStringIntoIterator {
    next_glyph_string: Option<GlyphStringRef>,
}

impl Iterator for GlyphStringIntoIterator {
    type Item = GlyphStringRef;

    fn next(&mut self) -> Option<GlyphStringRef> {
        let new_next = self.next_glyph_string.and_then(|n| {
            if n.next.is_null() {
                None
            } else {
                Some(GlyphStringRef::from(n.next))
            }
        });

        let result = self.next_glyph_string;
        self.next_glyph_string = new_next;

        result
    }
}

// Lisp Glyph Strings are the other side of the pipeline.  See `composite.h`.

// pub type LGString = LispObject;

/// A Lisp Glyph (LGLYPH).
struct LGlyph {
    ptr: LispObject
}


impl LGlyph {
    ///
    pub fn new(ptr: LispObject) -> LGlyph { LGlyph { ptr } }

    pub fn is_nil(&self) -> bool { self.ptr.is_nil() }

    fn adjustment_ptr(&self) -> LispObject {
        unsafe { AREF(self.ptr, LGLYPH_IX_ADJUSTMENT as isize)}
    }

    fn adjustment_value(&self, i: isize) -> EmacsInt {
        let adj = self.adjustment_ptr();
        if unsafe { VECTORP(adj) } {
            (unsafe {AREF(adj, i) }).as_fixnum_or_error()
        } else {
            0 as EmacsInt
        }
    }

    pub fn width(&self) -> EmacsInt {
        (unsafe { AREF(self.ptr, LGLYPH_IX_WIDTH as isize)}).as_fixnum_or_error()
    }

    pub fn has_adjustment(&self) -> bool { self.adjustment_ptr().is_nil() }

    fn x_offset(&self) -> EmacsInt {self.adjustment_value( 0)}

    fn y_offset(&self) -> EmacsInt {self.adjustment_value( 1)}

    fn width_adjustment(&self) -> EmacsInt {self.adjustment_value( 2)}
}

struct LGString {
    ptr: LispObject,
    idx: isize,

}

impl LGString {
    pub fn new(ptr: LispObject) -> LGString {
        LGString {
            ptr,
            idx: 0,
        }
    }

    pub fn from_id(id: isize) -> LGString {
        let ptr = unsafe { composition_gstring_from_id(id) };
        Self::new(ptr)
    }

    pub fn glyph(&self, idx: isize) -> LGlyph {
        LGlyph::new( unsafe { AREF(self.ptr, idx + 2) } )
    }
}

impl Iterator for LGString {
    type Item = LGlyph;

    // We don't keep a reference to the glyph.  Should we?
    fn next(&mut self) -> Option<Self::Item> {
        self.idx += 1;
        let next_glyph = self.glyph(self.idx);
        if next_glyph.is_nil() {
            None
        } else {
            Some(next_glyph)
        }
    }
}
