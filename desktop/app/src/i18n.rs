// SPDX-License-Identifier: GPL-3.0-or-later
//! Translation for the strings the Rust side builds.
//!
//! QML strings go through `qsTr()` and `lupdate` extracts them. These never
//! reach QML: they are built here and handed over as plain strings, so the
//! installed `QTranslator` only finds them if we look them up ourselves.
//!
//! Every pair used here must also appear in the `QT_TRANSLATE_NOOP` list at
//! the bottom of `cpp/app_helpers.cpp`. That list is what `lupdate` reads, so
//! without it a regenerated `.ts` drops the entry as obsolete and the string
//! silently falls back to English.

use crate::bridge::native::ffi;

/// Translates `source` in `context`.
pub fn tr(context: &str, source: &str) -> String {
    ffi::translate(context, source)
}

/// Translates a `%1`-style template and fills in the arguments. An untranslated
/// locale keeps the placeholders, so the substitution still has to happen.
pub fn tr_args(context: &str, source: &str, args: &[&str]) -> String {
    let mut out = tr(context, source);
    for (index, arg) in args.iter().enumerate() {
        out = out.replace(&format!("%{}", index + 1), arg);
    }
    out
}
