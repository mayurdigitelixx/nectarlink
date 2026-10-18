// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.clipboard

import app.nectarlink.android.clipboard.PhoneClipboard.ClipCandidate
import app.nectarlink.android.clipboard.PhoneClipboard.ItemChoice
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

class PhoneClipboardTest {

    @Test
    fun `selectClipItem prefers item text over URI even when clip description advertises an image`() {
        val choice = PhoneClipboard.selectClipItem(
            items = listOf(
                ClipCandidate(
                    text = "Caption text",
                    uri = "content://media/external/images/1",
                    uriMime = { "image/jpeg" },
                ),
            ),
            descHasImage = true,
        )
        assertEquals(ItemChoice.Text(index = 0, text = "Caption text", coerced = false), choice)
    }

    @Test
    fun `selectClipItem returns Image when item has an image URI and no direct text`() {
        val byResolverMime = PhoneClipboard.selectClipItem(
            items = listOf(
                ClipCandidate(
                    text = null,
                    uri = "content://media/external/images/2",
                    uriMime = { "image/png" },
                ),
            ),
            descHasImage = false,
        )
        assertEquals(ItemChoice.Image("content://media/external/images/2"), byResolverMime)

        val byClipDescription = PhoneClipboard.selectClipItem(
            items = listOf(
                ClipCandidate(
                    text = null,
                    uri = "content://provider/untyped-image",
                    uriMime = { null },
                ),
            ),
            descHasImage = true,
        )
        assertEquals(ItemChoice.Image("content://provider/untyped-image"), byClipDescription)
    }

    @Test
    fun `selectClipItem never coerces a non-image URI item to text`() {
        var coerceCalled = false
        val choice = PhoneClipboard.selectClipItem(
            items = listOf(
                ClipCandidate(
                    text = null,
                    uri = "content://downloads/document.pdf",
                    uriMime = { "application/pdf" },
                    coercedText = {
                        coerceCalled = true
                        "content://downloads/document.pdf"
                    },
                ),
            ),
            descHasImage = false,
        )
        assertEquals(ItemChoice.Empty, choice)
        assertFalse("coerceToText must not be called for URI-backed items", coerceCalled)
    }

    @Test
    fun `selectClipItem falls back to coercedText only for items without a URI`() {
        val choice = PhoneClipboard.selectClipItem(
            items = listOf(
                ClipCandidate<String>(
                    text = null,
                    uri = null,
                    coercedText = { "coerced intent text" },
                ),
            ),
            descHasImage = false,
        )
        assertEquals(ItemChoice.Text(index = 0, text = "coerced intent text", coerced = true), choice)

        val echoed = PhoneClipboard.selectClipItem(
            items = listOf(
                ClipCandidate<String>(
                    text = null,
                    uri = null,
                    coercedText = { "coerced intent text" },
                ),
            ),
            descHasImage = false,
            isTextEcho = { it == "coerced intent text" },
        )
        assertEquals(ItemChoice.Empty, echoed)
    }
}
