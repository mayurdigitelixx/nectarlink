// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.mirror

import android.Manifest
import android.app.Activity
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import app.nectarlink.android.NectarlinkApplication
import app.nectarlink.android.R
import app.nectarlink.android.elevated.Elevated

/** A PC asking for the screen (and its sound), with what it wants. */
data class MirrorRequest(
    val pcId: String,
    val maxSize: Int,
    val fps: Int,
    val bitrate: Int,
    val audio: Boolean,
    val stayAwake: Boolean = false,
    val screenOff: Boolean = false,
    /** The requester's generation, echoed in the stream's config (`null` predates correlation). */
    val request: ULong? = null,
    /**
     * The connection the request arrived on, set once by [MirrorRequests.show];
     * `null` shares and declines nothing.
     */
    val connection: ULong? = null,
) {
    fun toIntent(intent: Intent): Intent {
        intent
            .putExtra("pc", pcId)
            .putExtra("maxSize", maxSize)
            .putExtra("fps", fps)
            .putExtra("bitrate", bitrate)
            .putExtra("audio", audio)
            .putExtra("stayAwake", stayAwake)
            .putExtra("screenOff", screenOff)
        if (request != null) intent.putExtra("request", request.toLong())
        if (connection != null) intent.putExtra("connection", connection.toLong())
        return intent
    }

    companion object {
        fun of(intent: Intent): MirrorRequest? = intent.getStringExtra("pc")?.let {
            MirrorRequest(
                it,
                intent.getIntExtra("maxSize", 1920),
                intent.getIntExtra("fps", 60),
                intent.getIntExtra("bitrate", 8_000_000),
                intent.getBooleanExtra("audio", false),
                intent.getBooleanExtra("stayAwake", false),
                intent.getBooleanExtra("screenOff", false),
                if (intent.hasExtra("request")) intent.getLongExtra("request", 0L).toULong() else null,
                if (intent.hasExtra("connection")) intent.getLongExtra("connection", 0L).toULong() else null,
            )
        }
    }
}

/**
 * Asking the user: Android shows its screen capture prompt only from a
 * screen the user is on, so a PC's request waits in a notification; tapping
 * it opens [MirrorConsentActivity], which shows the prompt. While the app
 * is on screen the prompt opens right away, with or without notifications.
 *
 * Each PC waits on one request at a time: a newer one replaces it, the PC's
 * Stop forgets it, and the consent shares for that request alone, once.
 * Turned down or left unanswered, only that request is declined.
 */
object MirrorRequests {
    private const val CHANNEL = "mirror_requests"
    private const val TAG = "mirror"

    private val pending = HashMap<String, MirrorRequest>()
    /** Each PC's latest request timeout, until its prompt opens (guarded by [pending]). */
    private val expiries = HashMap<String, Runnable>()
    private val main by lazy { Handler(Looper.getMainLooper()) }

    internal fun ask(request: MirrorRequest) {
        synchronized(pending) { pending[request.pcId] = request }
    }

    internal fun isPending(request: MirrorRequest): Boolean =
        synchronized(pending) { pending[request.pcId] == request }

    /** Takes `request` if its PC still waits on it; true at most once per [ask]. */
    internal fun take(request: MirrorRequest): Boolean =
        synchronized(pending) { pending.remove(request.pcId, request) }

    internal fun forget(pcId: String) {
        synchronized(pending) { pending.remove(pcId) }
    }

    /**
     * Whether the PC can tap and type on this phone too, not just watch it:
     * only with the phone owner's grant for that PC, a way to deliver input,
     * no Stop, and a request generation and connection the input is tied to.
     */
    internal fun canControl(
        granted: Boolean,
        inputRunning: Boolean,
        elevatedRunning: Boolean,
        typingStopped: Boolean,
        correlated: Boolean,
    ): Boolean = granted && correlated && (inputRunning || elevatedRunning) && !typingStopped

    private fun core(context: Context) = (context.applicationContext as NectarlinkApplication).core

    /**
     * Whether the PC is still on the connection `request` arrived on and,
     * with a generation, the core still holds it as accepted there. A request
     * without one (a PC predating correlation) can still be viewed.
     */
    internal fun accepted(context: Context, request: MirrorRequest): Boolean {
        val core = core(context)
        val connection = request.connection
        return MirrorSessionPolicy.admitsCaptureOwner(
            request.request,
            connection,
            request.request?.let { core.mirrorCaptureContext(request.pcId, it) }?.connection,
            connection != null && core.mirrorConnectionCurrent(request.pcId, connection),
        )
    }

    /** Still the one its PC waits on, here and in the core. */
    internal fun isCurrent(context: Context, request: MirrorRequest): Boolean =
        isPending(request) && accepted(context, request)

    /** Tells the PC that this exact request was turned down; a newer one stays. */
    internal fun decline(context: Context, request: MirrorRequest) {
        val generation = request.request ?: return
        val connection = request.connection ?: return
        core(context).declineMirrorRequest(request.pcId, generation, connection)
    }

    /** Its prompt is open: the answer, not the timeout, settles `request`. */
    internal fun claim(request: MirrorRequest) {
        synchronized(pending) { if (pending[request.pcId] == request) expiries.remove(request.pcId) else null }
            ?.let { main.removeCallbacks(it) }
    }

    private fun expireLater(context: Context, request: MirrorRequest) {
        val expiry = Runnable { if (take(request)) decline(context, request) }
        synchronized(pending) { expiries.put(request.pcId, expiry) }?.let { main.removeCallbacks(it) }
        main.postDelayed(expiry, REQUEST_TIMEOUT_MS)
    }

    fun createChannel(context: Context) {
        context.getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(CHANNEL, context.getString(R.string.channel_mirror_requests), NotificationManager.IMPORTANCE_HIGH),
        )
    }

    /** How a request reaches the phone's owner; with neither, nobody is asked. */
    internal data class Delivery(val notify: Boolean, val open: Boolean) {
        val asks: Boolean get() = notify || open
    }

    /**
     * A notification only where the owner would see it: the app's
     * notifications and this channel on, and (Android 13 and later) the
     * permission granted. The prompt itself only while the app is on screen,
     * where Android lets it open; a background request never opens it.
     */
    internal fun delivery(
        sdk: Int,
        permissionGranted: Boolean,
        appNotificationsEnabled: Boolean,
        channelImportance: Int?,
        onScreen: Boolean,
    ): Delivery = Delivery(
        notify = (sdk < Build.VERSION_CODES.TIRAMISU || permissionGranted) && appNotificationsEnabled &&
            channelImportance != null && channelImportance != NotificationManager.IMPORTANCE_NONE,
        open = onScreen,
    )

    private fun onScreen(context: Context): Boolean {
        val am = context.getSystemService(android.app.ActivityManager::class.java)
        return am?.runningAppProcesses?.any {
            it.pid == android.os.Process.myPid() &&
                it.importance == android.app.ActivityManager.RunningAppProcessInfo.IMPORTANCE_FOREGROUND
        } == true
    }

    /** Only `request`'s own entry and timeout go; a newer request for its PC stays. */
    private fun withdraw(request: MirrorRequest) {
        claim(request)
        take(request)
    }

    /**
     * Shows the request that arrived on `connection`: a notification when the
     * owner can see one, and the prompt right away when the app is on screen.
     * False when neither reached the owner.
     */
    fun show(context: Context, received: MirrorRequest, pcName: String, connection: ULong?): Boolean {
        createChannel(context)
        val plan = delivery(
            sdk = Build.VERSION.SDK_INT,
            permissionGranted = android.os.Build.VERSION.SDK_INT < 33 ||
                ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED,
            appNotificationsEnabled = NotificationManagerCompat.from(context).areNotificationsEnabled(),
            channelImportance = context.getSystemService(NotificationManager::class.java)
                ?.getNotificationChannel(CHANNEL)?.importance,
            onScreen = onScreen(context),
        )
        if (!plan.asks) return false
        val request = if (received.connection != null) received else received.copy(connection = connection)
        ask(request)
        val state = core(context).state.value
        val control = canControl(
            granted = state.devices.firstOrNull { it.id == request.pcId }?.phoneControlEnabled == true,
            inputRunning = InputService.running,
            elevatedRunning = Elevated.running,
            typingStopped = request.pcId in state.typingStoppedPcIds,
            correlated = request.request != null && request.connection != null,
        )
        val activityIntent = request.toIntent(Intent(context, MirrorConsentActivity::class.java))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        val posted = plan.notify && runCatching {
            val open = PendingIntent.getActivity(
                context,
                request.pcId.hashCode(),
                activityIntent,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            val notification = NotificationCompat.Builder(context, CHANNEL)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle(context.getString(R.string.mirror_request_title, pcName.ifEmpty { context.getString(R.string.your_pc) }))
                .setContentText(context.getString(if (control) R.string.mirror_request_text_control else R.string.mirror_request_text))
                .setContentIntent(open)
                .setAutoCancel(true)
                .setTimeoutAfter(REQUEST_TIMEOUT_MS)
                .setCategory(NotificationCompat.CATEGORY_CALL)
                .setPriority(NotificationCompat.PRIORITY_HIGH)
                .build()
            NotificationManagerCompat.from(context).notify(TAG, request.pcId.hashCode(), notification)
        }.isSuccess
        // Before the prompt opens, so its claim always finds this timeout.
        expireLater(context.applicationContext, request)
        val opened = plan.open && runCatching { context.startActivity(activityIntent) }.isSuccess
        if (!posted && !opened) {
            // The PC hears this phone couldn't ask, so nothing may share for it.
            withdraw(request)
            return false
        }
        return true
    }

    /** The PC stopped asking. */
    fun dismiss(context: Context, pcId: String) {
        forget(pcId)
        synchronized(pending) { expiries.remove(pcId) }?.let { main.removeCallbacks(it) }
        hide(context, pcId)
    }

    /** Removes the notification; the request still waits for the prompt's answer. */
    internal fun hide(context: Context, pcId: String) {
        NotificationManagerCompat.from(context).cancel(TAG, pcId.hashCode())
    }

    /** A request nobody answered goes away after a while. */
    private const val REQUEST_TIMEOUT_MS = 2 * 60 * 1000L
}

/**
 * Shows Android's screen capture prompt for a PC's request, then starts
 * sharing. A PC that wants the sound too first gets the permission asked
 * for (once; without it, the screen is shared alone). A request its PC
 * stopped or replaced meanwhile shares nothing, whatever the answer, and
 * only the request this prompt is for is declined.
 */
class MirrorConsentActivity : ComponentActivity() {
    private var request: MirrorRequest? = null

    private val sound = registerForActivityResult(ActivityResultContracts.RequestPermission()) {
        val asked = request ?: return@registerForActivityResult finish()
        if (MirrorRequests.isCurrent(this, asked)) askForScreen() else drop(asked)
    }

    private val prompt = registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
        val data = result.data
        // Taken on a cancel too: the request is answered either way.
        val asked = request?.takeIf { MirrorRequests.take(it) }
        if (asked != null) {
            if (result.resultCode == Activity.RESULT_OK && data != null) {
                if (MirrorRequests.accepted(this, asked)) {
                    MirrorService.start(this, asked, result.resultCode, data, asked.connection)
                }
            } else {
                MirrorRequests.decline(this, asked)
            }
        }
        finish()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        request = MirrorRequest.of(intent)
        val asked = request ?: return finish()
        MirrorRequests.hide(this, asked.pcId)
        if (savedInstanceState != null) return
        if (!MirrorRequests.isCurrent(this, asked)) return drop(asked)
        MirrorRequests.claim(asked)
        val wantsSound = asked.audio && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q &&
            ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED
        if (wantsSound) sound.launch(Manifest.permission.RECORD_AUDIO) else askForScreen()
    }

    /** Stopped, replaced or answered: nothing to ask or decline. */
    private fun drop(asked: MirrorRequest) {
        MirrorRequests.take(asked)
        finish()
    }

    private fun askForScreen() {
        val projections = getSystemService(MediaProjectionManager::class.java)
        // The PC's taps land on the whole display: ask Android for the default display.
        val consent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            projections.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay())
        } else {
            projections.createScreenCaptureIntent()
        }
        prompt.launch(consent)
    }
}
