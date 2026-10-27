// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.notifications

import java.util.UUID

/** Only observed active notifications own action IDs. Eviction fails closed. */
internal class NotificationActionRegistry<T>(private val capacity: Int = 512) {
    internal class Revision<T>(val key: String, val id: String, val value: T) {
        internal var offered: Set<Int> = emptySet()
    }
    private val entries = LinkedHashMap<String, Revision<T>>()

    init { require(capacity > 0) }

    @Synchronized
    fun posted(key: String, value: T): Revision<T> {
        val revision = Revision(key, UUID.randomUUID().toString(), value)
        entries.remove(key)
        entries[key] = revision
        while (entries.size > capacity) entries.remove(entries.keys.first())
        return revision
    }

    @Synchronized
    fun current(revision: Revision<T>): Boolean = entries[revision.key] === revision

    /** Bind only the action indices actually returned by the eligible reader. */
    @Synchronized
    fun offer(revision: Revision<T>, indices: Set<Int>): Boolean {
        if (!current(revision)) return false
        revision.offered = indices.filter { it >= 0 }.toSet()
        return true
    }

    @Synchronized
    fun remove(key: String) { entries.remove(key) }

    @Synchronized
    fun clear() { entries.clear() }

    /** Hold the same monitor through dispatch, ordering it against callback invalidation. */
    @Synchronized
    fun useAction(key: String, id: String, run: (T, Int) -> Unit): Boolean {
        val revision = entries[key] ?: return false
        val separator = id.lastIndexOf(':')
        if (separator < 0 || id.substring(0, separator) != revision.id) return false
        val suffix = id.substring(separator + 1)
        val index = suffix.toIntOrNull()?.takeIf { it >= 0 && it.toString() == suffix } ?: return false
        if (index !in revision.offered) return false
        run(revision.value, index)
        return true
    }
}
