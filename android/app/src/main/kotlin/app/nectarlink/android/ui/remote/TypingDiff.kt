// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.ui.remote

/**
 * What changed in the phone's typing field, as keys for the PC: delete
 * [backspaces] characters at the cursor, then type [added]. The field keeps
 * what was typed so the keyboard can autocorrect and stay on its symbols
 * page; a word it corrects ("teh" to "the") comes out as backspaces and the
 * new word.
 */
internal data class TypingDiff(val backspaces: Int, val added: String)

internal fun typingDiff(old: String, new: String): TypingDiff {
    var same = 0
    val shorter = minOf(old.length, new.length)
    while (same < shorter && old[same] == new[same]) same++
    // Never split an emoji (or any character outside the BMP) in two.
    if (same > 0 && Character.isHighSurrogate(old[same - 1]) && same < old.length) same--
    val removed = old.substring(same)
    return TypingDiff(
        backspaces = removed.codePointCount(0, removed.length),
        added = new.substring(same),
    )
}
