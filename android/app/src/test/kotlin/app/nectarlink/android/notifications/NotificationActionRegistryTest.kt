// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.notifications

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class NotificationActionRegistryTest {
    @Test
    fun exactCurrentRevisionAndActionReachTheRegisteredTarget() {
        val registry = NotificationActionRegistry<String>()
        val revision = registry.posted("key", "registered-target")
        assertTrue(registry.offer(revision, setOf(2)))
        var result: Pair<String, Int>? = null
        assertTrue(registry.useAction("key", "${revision.id}:2") { target, index -> result = target to index })
        assertEquals("registered-target" to 2, result)
        assertTrue(registry.current(revision))
    }

    @Test
    fun replacementRemovalRefreshAndEvictionNeverDispatchStaleActions() {
        val registry = NotificationActionRegistry<String>(capacity = 1)
        val old = registry.posted("key", "old-target")
        assertTrue(registry.offer(old, setOf(0)))
        var dispatches = 0
        val current = registry.posted("key", "replacement")
        assertTrue(registry.offer(current, setOf(0)))
        assertFalse(registry.offer(old, setOf(0)))
        assertFalse(registry.current(old))
        assertFalse(registry.useAction("key", "${old.id}:0") { _, _ -> dispatches++ })
        assertFalse(registry.useAction("wrong-key", "${current.id}:0") { _, _ -> dispatches++ })
        assertFalse(registry.useAction("key", "${current.id}:-1") { _, _ -> dispatches++ })
        assertFalse(registry.useAction("key", "${current.id}:1") { _, _ -> dispatches++ })
        // The reader offers at most the five actions Core keeps on the wire.
        val offered = (0 until 6).take(NotificationReader.MAX_ACTIONS).toSet()
        assertTrue(registry.offer(current, offered))
        assertFalse(registry.useAction("key", "${current.id}:5") { _, _ -> dispatches++ })
        registry.remove("key")
        assertFalse(registry.useAction("key", "${current.id}:0") { _, _ -> dispatches++ })
        val refreshed = registry.posted("key", "after-remove")
        registry.clear()
        assertFalse(registry.useAction("key", "${refreshed.id}:0") { _, _ -> dispatches++ })
        val evicted = registry.posted("key", "evicted")
        registry.posted("other", "retained")
        assertFalse(registry.useAction("key", "${evicted.id}:0") { _, _ -> dispatches++ })
        assertEquals(0, dispatches)
    }
}
