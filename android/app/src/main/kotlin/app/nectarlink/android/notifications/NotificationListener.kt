// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.notifications

import android.app.AppOpsManager
import android.app.Notification
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Process
import android.provider.Settings
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import androidx.core.app.NotificationManagerCompat
import app.nectarlink.android.NectarlinkApplication
import app.nectarlink.core.NotificationFailure

/**
 * Hands this phone's notifications to the core, which mirrors them to the
 * PCs the user allows (docs/protocol/notifications.md), and carries out
 * what a PC asks for: dismissing, replying, running actions.
 *
 * Runs only while the user has granted notification access; Android binds
 * it (and keeps the process alive) for as long as that lasts.
 */
class NotificationListener : NotificationListenerService() {
    private val core get() = (application as NectarlinkApplication).core
    private val reader by lazy { NotificationReader(this) }
    private val actionRevisions = NotificationActionRegistry<StatusBarNotification>()
    private val limiter = LiveRateLimiter { notification -> core.notificationPosted(notification) }
    private val worker = java.util.concurrent.Executors.newSingleThreadExecutor { r ->
        Thread(r, "nectarlink-notif-reader").apply { isDaemon = true }
    }

    /** Keys already sent, so an update that shouldn't alert again doesn't. */
    private val sent = java.util.Collections.synchronizedSet(mutableSetOf<String>())

    override fun onListenerConnected() {
        instance = this
        if (isPcActive()) applyHostHints(true)
        val snapshot = actionSnapshot() ?: emptyList()
        work {
            val ranking = runCatching { currentRanking }.getOrNull()
            limiter.clear()
            sent.clear()
            val items = snapshot.mapNotNull { revision ->
                mirrored(revision, ranking, update = false)
                    ?.takeIf { actionRevisions.current(revision) }
            }
            sent.addAll(items.map { it.key })
            core.notificationAccessChanged(true, items)
        }
    }

    override fun onListenerDisconnected() {
        actionRevisions.clear()
        if (instance === this) instance = null
        work {
            limiter.clear()
            sent.clear()
            core.notificationAccessChanged(false, emptyList())
        }
    }

    override fun onNotificationPosted(sbn: StatusBarNotification, rankingMap: RankingMap?) {
        val key = sbn.key
        // Invalidate before queued reader work: a stale desktop click must not
        // resolve an index against the replacement notification.
        val revision = actionRevisions.posted(key, sbn)
        work {
            if (!actionRevisions.current(revision)) return@work
            val notification = mirrored(revision, rankingMap, update = key in sent)
            if (!actionRevisions.current(revision)) return@work
            if (notification == null) {
                // Something we mirrored may have turned into something we don't
                // (e.g. it became ongoing); take it away on the PC.
                limiter.cancel(key)
                if (sent.remove(key)) core.notificationRemoved(key)
                return@work
            }
            sent.add(key)
            limiter.onPosted(notification)
        }
    }

    override fun onNotificationRemoved(sbn: StatusBarNotification) {
        val key = sbn.key
        actionRevisions.remove(key)
        work {
            limiter.cancel(key)
            if (sent.remove(key)) core.notificationRemoved(key)
        }
    }

    override fun onDestroy() {
        actionRevisions.clear()
        if (instance === this) instance = null
        // Android's onDestroy calls onListenerDisconnected, which still needs
        // the worker; shut it down after (queued work still runs).
        super.onDestroy()
        worker.shutdown()
    }

    /**
     * Runs [task] on the worker. Android can still deliver a callback after
     * onDestroy (one queued just before it); that one is dropped, since the
     * disconnect already told the core notification access is gone.
     */
    private fun work(task: () -> Unit) {
        try {
            worker.execute(task)
        } catch (_: java.util.concurrent.RejectedExecutionException) {
        }
    }

    internal fun applyHostHints(suppressPhoneEffects: Boolean) {
        runCatching {
            requestListenerHints(if (suppressPhoneEffects) HINT_HOST_DISABLE_NOTIFICATION_EFFECTS else 0)
        }
    }

    /** Re-reads active notifications after Elevated grants sensitive notification access. */
    internal fun refreshActive() = refreshSnapshot()

    /**
     * Retires action revisions before replacing the mirrored snapshot.
     */
    private fun refreshSnapshot() {
        // Retire every offered action synchronously, even if the snapshot query fails.
        val snapshot = actionSnapshot()
        work {
            limiter.clear()
            if (snapshot == null) return@work
            val ranking = runCatching { currentRanking }.getOrNull()
            val items = snapshot.mapNotNull { revision ->
                mirrored(revision, ranking, update = true)
                    ?.takeIf { actionRevisions.current(revision) }
            }
            sent.clear()
            sent.addAll(items.map { it.key })
            core.notificationsRefreshed(items)
        }
    }

    /**
     * Structural and Android-user eligibility is checked before reading content.
     * Only actions actually offered to the peer are admitted later.
     */
    private fun mirrored(
        revision: NotificationActionRegistry.Revision<StatusBarNotification>,
        ranking: RankingMap?,
        update: Boolean,
    ): app.nectarlink.core.Notification? {
        if (!actionRevisions.current(revision)) return null
        val sbn = revision.value
        if (!eligible(sbn)) return null
        val notification = reader.read(sbn, ranking, update, revision.id) ?: return null
        val offered = notification.actions.mapNotNull { it.id.substringAfterLast(':').toIntOrNull() }.toSet()
        if (!actionRevisions.offer(revision, offered)) return null
        return notification
    }

    /** Preserve the reader's structural policy and reject other Android users. */
    private fun eligible(sbn: StatusBarNotification): Boolean =
        sbn.user == android.os.Process.myUserHandle() && reader.actionEligible(sbn)

    /** Capture metadata before queueing reads, serialized with post/removal invalidation. */
    private fun actionSnapshot(): List<NotificationActionRegistry.Revision<StatusBarNotification>>? =
        synchronized(actionRevisions) {
            actionRevisions.clear()
            val snapshot = runCatching { activeNotifications?.toList().orEmpty() }.getOrNull()
                ?: return@synchronized null
            snapshot.map { actionRevisions.posted(it.key, it) }
        }

    /** Resolve only an action actually offered for this still-current observed revision. */
    internal fun withCurrentAction(key: String, id: String, execute: (Notification.Action) -> Unit) {
        val found = actionRevisions.useAction(key, id) { observed, index ->
            if (instance !== this) throw NotificationFailure.Unsupported()
            val current = find(key) ?: throw NotificationFailure.NotFound()
            if (current.key != observed.key || current.packageName != observed.packageName ||
                (Build.VERSION.SDK_INT >= 29 && current.uid != observed.uid) || current.user != observed.user ||
                current.postTime != observed.postTime || !eligible(current)
            ) throw NotificationFailure.NotFound()
            val offered = observed.notification.actions?.getOrNull(index) ?: throw NotificationFailure.NotFound()
            val target = current.notification.actions?.getOrNull(index) ?: throw NotificationFailure.NotFound()
            if (offered.actionIntent == null || target.actionIntent != offered.actionIntent ||
                target.title?.toString() != offered.title?.toString() ||
                inputIdentity(target) != inputIdentity(offered)
            ) throw NotificationFailure.NotFound()
            execute(target)
        }
        if (!found) throw NotificationFailure.NotFound()
    }

    private fun inputIdentity(action: Notification.Action): List<Pair<String, Boolean>> =
        action.remoteInputs.orEmpty().map { it.resultKey to it.allowFreeFormInput }

    /** What a PC asks for, on the listener that's connected now. */
    internal fun dismiss(key: String) = cancelNotification(key)

    internal fun find(key: String): StatusBarNotification? =
        runCatching { getActiveNotifications(arrayOf(key)) }.getOrNull()?.firstOrNull()

    companion object {
        /** The connected listener, if notification access is granted. */
        @Volatile
        internal var instance: NotificationListener? = null
            private set

        @Volatile
        private var pcActiveUntilMs: Long = 0L
        private val mainHandler = android.os.Handler(android.os.Looper.getMainLooper())
        private val expirePcActive = Runnable {
            if (!isPcActive()) {
                instance?.applyHostHints(false)
            }
        }

        fun isPcActive(): Boolean =
            android.os.SystemClock.elapsedRealtime() < pcActiveUntilMs

        /**
         * Updates whether the user is actively using a paired PC (`pc_active`
         * toggle). When active, Android's `HINT_HOST_DISABLE_NOTIFICATION_EFFECTS`
         * suppresses redundant phone notification sound/vibration effects while
         * still delivering notifications to the PC.
         */
        fun setPcActive(active: Boolean) {
            mainHandler.removeCallbacks(expirePcActive)
            if (active) {
                pcActiveUntilMs = android.os.SystemClock.elapsedRealtime() + 65_000L
                instance?.applyHostHints(true)
                mainHandler.postDelayed(expirePcActive, 66_000L)
            } else {
                pcActiveUntilMs = 0L
                instance?.applyHostHints(false)
            }
        }

        /** Whether the user granted notification access to this app. */
        fun hasAccess(context: Context): Boolean =
            NotificationManagerCompat.getEnabledListenerPackages(context).contains(context.packageName)

        /**
         * Whether this listener may read notifications Android 15+ marks
         * sensitive (such as one-time verification codes). Before Android 15
         * listeners aren't redacted by this app-op.
         */
        fun canReadSensitive(context: Context): Boolean {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.VANILLA_ICE_CREAM) return true
            val ops = context.getSystemService(AppOpsManager::class.java) ?: return false
            return runCatching {
                // Deprecated on 37 for attribution-aware checks; this one is only the app's own.
                @Suppress("DEPRECATION")
                ops.unsafeCheckOpNoThrow(OP_SENSITIVE, Process.myUid(), context.packageName) == AppOpsManager.MODE_ALLOWED
            }.getOrDefault(false)
        }

        /** The app-op's name for `appops set`. */
        const val OP_SENSITIVE_NAME = "RECEIVE_SENSITIVE_NOTIFICATIONS"
        private const val OP_SENSITIVE = "android:receive_sensitive_notifications"

        fun component(context: Context) = ComponentName(context, NotificationListener::class.java)

        /** Android's notification access screen, on NectarLink's own switch where possible. */
        fun settingsIntent(context: Context): Intent =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                Intent(Settings.ACTION_NOTIFICATION_LISTENER_DETAIL_SETTINGS).putExtra(
                    Settings.EXTRA_NOTIFICATION_LISTENER_COMPONENT_NAME,
                    component(context).flattenToString(),
                )
            } else {
                Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS)
            }.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
    }
}
