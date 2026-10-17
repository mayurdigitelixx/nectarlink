// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.webcam

import org.junit.Assert.assertEquals
import org.junit.Test

class ResolutionTest {
    @Test
    fun theResolutionComesFromTheShortSide() {
        assertEquals(720, resolutionOf(1280, 720))
        assertEquals(1080, resolutionOf(1920, 1080))
        // Upright, the phone streams portrait.
        assertEquals(1080, resolutionOf(1080, 1920))
        assertEquals(2160, resolutionOf(3840, 2160))
        assertEquals(2160, resolutionOf(2160, 3840))
        // Encoders round to their block size.
        assertEquals(1080, resolutionOf(1920, 1088))
    }
}
