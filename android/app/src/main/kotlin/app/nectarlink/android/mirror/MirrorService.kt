// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.mirror

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import androidx.core.content.IntentCompat
import app.nectarlink.android.NectarlinkApplication
import app.nectarlink.android.R
import java.util.concurrent.atomic.AtomicLong
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

@androidx.compose.runtime.Immutable
data class MirrorSession(
    val active: Boolean = false,
    val pcId: String? = null,
    val audio: Boolean = false,
    val stayAwake: Boolean = false,
    val screenOff: Boolean = false,
)

/**
 * Shares the screen with one PC while the user agreed: holds the screen
 * capture (Android wants a foreground service for it, with its own
 * notification and a Stop button) and runs the encoder.
 */
class MirrorService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val main = Handler(Looper.getMainLooper())
    private var projection: MediaProjection? = null
    private var encoder: ScreenEncoder? = null
    @Volatile private var elevatedSocket: java.net.Socket? = null
    /** Opening the helper's screen stream (off the main thread), before [elevatedSocket] is set. */
    @Volatile private var elevatedStarting = false
    private var elevatedDisplayListener: android.hardware.display.DisplayManager.DisplayListener? = null
    private var sound: SoundCapture? = null
    private var pcId: String? = null
    /** The live capture's request generation (`null` predates correlation); stale jobs compare against it. */
    @Volatile private var activeRequest: ULong? = null
    @Volatile private var activeConnection: ULong? = null
    @Volatile private var controlAllowed = false
    /**
     * Service-local capture generation, incremented on every [tearDownCapture]
     * (main thread only). Fences jobs whose (pc, request) could match again
     * after a stop/restart or a legacy `null` request, and fences the old
     * MediaProjection callback queued by `projection.stop()`.
     */
    private var captureEpoch = 0L
    /**
     * The newest start delivered (main thread only). Stopping through it, not
     * unconditionally, keeps a start sent after it (a Mac reconnect) alive
     * when the old capture's stream ends first.
     */
    private var lastStartId = 0
    private var stayAwake: Boolean = false
    private var screenOff: Boolean = false

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        lastStartId = startId
        if (intent?.action == ACTION_STOP) {
            stopByUser()
            return START_NOT_STICKY
        }
        val pc = intent?.getStringExtra(EXTRA_PC)
        val isElevated = intent?.getBooleanExtra(EXTRA_ELEVATED, false) == true
        val consent = intent?.let { IntentCompat.getParcelableExtra(it, EXTRA_CONSENT, Intent::class.java) }
        val requestToken = if (intent?.hasExtra(EXTRA_REQUEST) == true) intent.getLongExtra(EXTRA_REQUEST, 0L).toULong() else null
        val originalConnection = if (intent?.hasExtra(EXTRA_CONNECTION) == true) intent.getLongExtra(EXTRA_CONNECTION, 0L).toULong() else null
        val issue = intent?.getLongExtra(EXTRA_ISSUE, 0L) ?: 0L
        val busy = projection != null || elevatedSocket != null || elevatedStarting
        if (pc == null || (!isElevated && consent == null) || !MirrorSessionPolicy.admitsStart(issue, stoppedThrough)) {
            // A start sent before a Stop on this phone: the Stop wins, and a
            // capture started after it stays.
            if (!busy) stopSelf(startId)
            return START_NOT_STICKY
        }
        val core = (application as NectarlinkApplication).core
        val captureOwner = requestToken?.let { core.mirrorCaptureContext(pc, it) }
        if (!MirrorSessionPolicy.admitsCaptureOwner(
                requestToken, originalConnection, captureOwner?.connection,
                originalConnection?.let { core.mirrorConnectionCurrent(pc, it) } == true,
            )) {
            // Consent/start from a stopped, unpaired, replaced or older connection.
            // Leave any later live capture intact.
            if (!busy) stopSelf(startId)
            return START_NOT_STICKY
        }
        if (busy && !MirrorSessionPolicy.shouldPreempt(pcId, activeRequest, pc, requestToken)) {
            // A duplicate delivery, a superseded generation, or another PC:
            // the live capture stays.
            return START_NOT_STICKY
        }
        if (busy) {
            // A replacement generation from the same PC (a Mac reconnect):
            // retire the stale capture first, so its jobs can't outlive or
            // reopen this request.
            tearDownCapture()
        }
        val name = core.state.value.nameOf(pc).orEmpty()
        createChannel(this)
        val maxSize = intent.getIntExtra(EXTRA_MAX_SIZE, 1920)
        val fps = intent.getIntExtra(EXTRA_FPS, 60)
        val bitrate = intent.getIntExtra(EXTRA_BITRATE, 8_000_000)
        val audio = intent.getBooleanExtra(EXTRA_AUDIO, false)
        stayAwake = intent.getBooleanExtra(EXTRA_STAY_AWAKE, false)
        screenOff = intent.getBooleanExtra(EXTRA_SCREEN_OFF, false)

        if (isElevated) {
            val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE else 0
            ServiceCompat.startForeground(this, NOTIFICATION_ID, notification(name), type)
            pcId = pc
            activeRequest = requestToken
            activeConnection = originalConnection
            controlAllowed = true
            val myEpoch = captureEpoch
            current = this
            _session.value = MirrorSession(
                active = true,
                pcId = pc,
                audio = audio,
                stayAwake = stayAwake,
                screenOff = screenOff,
            )
            val fallbackRequest = MirrorRequest(pc, maxSize, fps, bitrate, audio, stayAwake, screenOff, requestToken)
            elevatedStarting = true
            startElevatedStream(core, pc, name, fallbackRequest, myEpoch, originalConnection)
            return START_NOT_STICKY
        }

        // The screen capture needs the service in the foreground first.
        val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION else 0
        ServiceCompat.startForeground(this, NOTIFICATION_ID, notification(name), type)
        val manager = getSystemService(MediaProjectionManager::class.java)
        val granted = runCatching {
            manager.getMediaProjection(intent.getIntExtra(EXTRA_RESULT, 0), consent!!)
        }.getOrNull()
        if (granted == null) {
            Log.w(TAG, "no screen capture")
            stopSharing()
            return START_NOT_STICKY
        }
        projection = granted
        if (!MirrorSessionPolicy.admitsCaptureOwner(
                requestToken, originalConnection, requestToken?.let { core.mirrorCaptureContext(pc, it)?.connection },
                originalConnection?.let { core.mirrorConnectionCurrent(pc, it) } == true,
            )) {
            stopSharing()
            return START_NOT_STICKY
        }
        pcId = pc
        activeRequest = requestToken
        activeConnection = originalConnection
        controlAllowed = true
        current = this
        _session.value = MirrorSession(
            active = true,
            pcId = pc,
            audio = audio,
            stayAwake = stayAwake,
            screenOff = screenOff,
        )
        // Fenced below: a replacement request retires this job before its
        // stream attaches, and its failure must not stop the replacement.
        // The epoch also fences the OLD projection callback queued by
        // tearDownCapture's projection.stop().
        val myPc = pc
        val myRequest = requestToken
        val myEpoch = captureEpoch
        // A resize may arrive before the async stream opens; retain it for
        // this projection only and hand it to this capture's encoder on main.
        var capturedSize: Pair<Int, Int>? = null
        granted.registerCallback(object : MediaProjection.Callback() {
            override fun onCapturedContentResize(width: Int, height: Int) {
                if (projection !== granted ||
                    !MirrorSessionPolicy.isLive(captureEpoch, pcId, activeRequest, myEpoch, myPc, myRequest) ||
                    width <= 0 || height <= 0
                ) return
                capturedSize = width to height
                encoder?.capturedContentResized(width, height)
            }

            // The user stopped it (from the status bar, or by locking).
            // Fenced: tearDownCapture's projection.stop() queues the OLD
            // callback, which must not kill the replacement.
            override fun onStop() {
                if (MirrorSessionPolicy.isLive(captureEpoch, pcId, activeRequest, myEpoch, myPc, myRequest)) {
                    controlAllowed = false
                }
                main.post {
                    if (!MirrorSessionPolicy.isLive(captureEpoch, pcId, activeRequest, myEpoch, myPc, myRequest)) return@post
                    stopByUser()
                }
            }
        }, main)
        scope.launch {
            val stream = runCatching { core.mirrorOpen(pc) }.getOrElse {
                Log.w(TAG, "can't stream to the PC", it)
                main.post { if (MirrorSessionPolicy.isLive(captureEpoch, pcId, activeRequest, myEpoch, myPc, myRequest)) stopSharing() }
                return@launch
            }
            main.post {
                if (!MirrorSessionPolicy.isLive(captureEpoch, pcId, activeRequest, myEpoch, myPc, myRequest)) {
                    stream.end()
                    return@post
                }
                val running = projection
                if (running == null) {
                    stream.end()
                    return@post
                }
                encoder = ScreenEncoder(this@MirrorService, running, stream, maxSize, fps, bitrate, request = myRequest) {
                    main.post { if (MirrorSessionPolicy.isLive(captureEpoch, pcId, activeRequest, myEpoch, myPc, myRequest)) stopSharing() }
                }.also { started ->
                    capturedSize?.let { (width, height) -> started.capturedContentResized(width, height) }
                    started.start()
                }
                applyPower()
            }
            if (audio && SoundCapture.canCapture(this@MirrorService)) startSound(core, myPc, myRequest, myEpoch)
        }
        return START_NOT_STICKY
    }

    private fun startElevatedStream(
        core: app.nectarlink.android.core.Core,
        pc: String,
        pcName: String,
        request: MirrorRequest,
        myEpoch: Long,
        originalConnection: ULong?,
    ) = scope.launch {
        val myPc = pc
        val myRequest = request.request
        // The helper's socket: off the main thread, where Android refuses
        // network calls (and the phone then asked for screen-capture consent).
        val (initW, initH, initDpi) = ScreenEncoder.computeScreenSize(this@MirrorService, request.maxSize)
        val codec = ScreenEncoder.selectCodec("h264")
        val socket = app.nectarlink.android.elevated.Elevated.openScreen(
            initW,
            initH,
            initDpi,
            request.bitrate,
            request.fps,
            codec,
        )
        // Ownership handoff on main, atomically: the fence check and the
        // elevatedStarting/elevatedSocket assignment run together, so a
        // replacement's tearDownCapture cannot interleave between them.
        main.post {
            if (!MirrorSessionPolicy.isLive(captureEpoch, pcId, activeRequest, myEpoch, myPc, myRequest)) {
                runCatching { socket?.close() }
                return@post
            }
            elevatedStarting = false
            if (socket == null) {
                stopSharing()
                MirrorRequests.show(this@MirrorService, request, pcName, originalConnection)
                return@post
            }
            elevatedSocket = socket
            watchDisplay(socket, request, initW, initH, initDpi)
            scope.launch { streamElevated(core, myPc, pcName, request, socket, codec, myEpoch, originalConnection) }
        }
    }

    /** Resizes the helper's picture when the phone's screen turns. */
    private fun watchDisplay(
        socket: java.net.Socket,
        request: MirrorRequest,
        initW: Int,
        initH: Int,
        initDpi: Int,
    ) {
        if (elevatedSocket !== socket) return
        var curW = initW
        var curH = initH
        var curDpi = initDpi
        val updates = CaptureGeometryGate()
        val displays = getSystemService(android.hardware.display.DisplayManager::class.java)
        fun refresh() {
            val (nw, nh, ndpi) = ScreenEncoder.computeScreenSize(this@MirrorService, request.maxSize)
            if (nw == curW && nh == curH && ndpi == curDpi) return
            curW = nw
            curH = nh
            curDpi = ndpi
            val update = updates.submit(nw, nh, ndpi)
            Thread {
                runCatching {
                    updates.publish(update, socket) {
                        if (elevatedSocket !== socket) return@publish
                        socket.getOutputStream().apply {
                            write("R ${it.width} ${it.height} ${it.dpi}\n".toByteArray())
                            flush()
                        }
                    }
                }
            }.start()
        }
        val listener = object : android.hardware.display.DisplayManager.DisplayListener {
            override fun onDisplayChanged(displayId: Int) {
                if (displayId != android.view.Display.DEFAULT_DISPLAY) return
                refresh()
            }
            override fun onDisplayAdded(displayId: Int) {}
            override fun onDisplayRemoved(displayId: Int) {}
        }
        elevatedDisplayListener = listener
        displays?.registerDisplayListener(listener, main)
        // Registration does not replay a change that happened while the helper started.
        refresh()
        applyPower()
    }

    /** The helper's packets to the PC, until either side ends; asks for consent if none came. */
    private suspend fun streamElevated(
        core: app.nectarlink.android.core.Core,
        pc: String,
        pcName: String,
        request: MirrorRequest,
        socket: java.net.Socket,
        codec: String,
        myEpoch: Long,
        originalConnection: ULong?,
    ) {
        val stream = runCatching { core.mirrorOpen(pc) }.getOrElse {
            Log.w(TAG, "can't stream to the PC", it)
            main.post { if (MirrorSessionPolicy.isLive(captureEpoch, pcId, activeRequest, myEpoch, pc, request.request)) stopSharing() }
            return
        }
        if (request.audio && SoundCapture.canCapture(this@MirrorService)) startSound(core, pc, request.request, myEpoch)
        var gotConfig = false
        try {
            val input = java.io.DataInputStream(socket.getInputStream().buffered(1 shl 16))
            while (elevatedSocket === socket) {
                val length = input.readInt()
                if (length < 9 || length > 16 * 1024 * 1024) break
                val kind = input.readUnsignedByte()
                val time = input.readLong().toULong()
                val data = ByteArray(length - 9).also { input.readFully(it) }
                val result = when (kind) {
                    app.nectarlink.android.elevated.AppDisplay.KIND_CONFIG -> {
                        gotConfig = true
                        val fields = java.nio.ByteBuffer.wrap(data)
                        val (w, h) = fields.int to fields.int
                        stream.send(
                            app.nectarlink.core.VideoPacketKind.CONFIG,
                            0u,
                            app.nectarlink.core.mirrorConfigCodec(codec, w.toUInt(), h.toUInt(), 0u, request.request),
                        )
                    }
                    app.nectarlink.android.elevated.AppDisplay.KIND_KEYFRAME ->
                        stream.send(app.nectarlink.core.VideoPacketKind.KEYFRAME, time, data)
                    else ->
                        stream.send(app.nectarlink.core.VideoPacketKind.FRAME, time, data)
                }
                when (result) {
                    app.nectarlink.core.MirrorSendResult.CLOSED -> break
                    app.nectarlink.core.MirrorSendResult.NEED_KEYFRAME,
                    app.nectarlink.core.MirrorSendResult.DROPPED -> requestElevatedKeyframe()
                    app.nectarlink.core.MirrorSendResult.QUEUED -> {}
                }
            }
        } catch (_: java.io.EOFException) {
        } catch (e: java.io.IOException) {
            Log.d(TAG, "Elevated screen mirror ended", e)
        } finally {
            stream.end()
            val fallback = !gotConfig
            main.post {
                // A superseded job ends its own stream and nothing else: the
                // replacement's capture stays up, with no stale consent.
                if (!MirrorSessionPolicy.isLive(captureEpoch, pcId, activeRequest, myEpoch, pc, request.request)) return@post
                stopSharing()
                if (fallback) {
                    MirrorRequests.show(this@MirrorService, request, pcName, originalConnection)
                }
            }
        }
    }

    private fun requestElevatedKeyframe() {
        val sock = elevatedSocket ?: return
        runCatching {
            synchronized(sock) {
                sock.getOutputStream().apply {
                    write("K\n".toByteArray())
                    flush()
                }
            }
        }
    }

    private fun applyPower() {
        val active = (encoder != null && projection != null) || elevatedSocket != null
        StreamPower.set(this, StreamPower.SCREEN, active, stayAwake, screenOff)
    }

    /** The sound, on a stream of its own; the screen goes on without it if it can't. */
    private suspend fun startSound(core: app.nectarlink.android.core.Core, pc: String, request: ULong?, myEpoch: Long) {
        if (!SoundCapture.canCapture(this)) return
        val stream = runCatching { core.mirrorOpenAudio(pc) }.getOrElse {
            Log.w(TAG, "can't send the sound", it)
            return
        }
        main.post {
            if (!MirrorSessionPolicy.isLive(captureEpoch, pcId, activeRequest, myEpoch, pc, request)) {
                stream.end()
                return@post
            }
            val running = projection
            if ((running == null && !app.nectarlink.android.elevated.Elevated.running) || !MirrorSessionPolicy.isLive(captureEpoch, pcId, activeRequest, myEpoch, pc, request)) {
                stream.end()
                return@post
            }
            sound = SoundCapture(running, stream).also { it.start() }
        }
    }

    private fun notification(pcName: String): Notification {
        val stop = PendingIntent.getService(
            this, 0, Intent(this, MirrorService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return NotificationCompat.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.mirror_sharing, pcName.ifEmpty { getString(R.string.your_pc) }))
            .setOngoing(true)
            .setSilent(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .addAction(0, getString(R.string.action_stop), stop)
            .build()
    }

    private fun stopSharing() {
        controlAllowed = false
        stayAwake = false
        screenOff = false
        _session.value = MirrorSession()
        StreamPower.set(this, StreamPower.SCREEN, streaming = false)
        tearDownCapture()
        if (current === this) current = null
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
        stopSelf(lastStartId)
    }

    /**
     * A Stop from this phone's user (notification, app, or revoking the
     * capture): also refuses the starts already sent, so none brings the
     * capture back. Only while sharing: a start sent then reached a
     * foreground service, so it may be dropped without `startForeground`.
     */
    private fun stopByUser() {
        controlAllowed = false
        if (current === this) {
            stoppedThrough = issued.get()
            val pc = pcId
            val request = activeRequest
            val connection = activeConnection
            if (pc != null && request != null && connection != null) {
                (application as NectarlinkApplication).core.declineMirrorRequest(pc, request, connection)
            }
        }
        stopSharing()
    }

    /**
     * Retires the capture (projection, encoder, helper socket and sound)
     * without stopping the service: a replacement request starts its own
     * capture right after. Also clears a still-starting job's flag, so its
     * late completion finds a new generation and ends itself. Runs on main;
     * bumps [captureEpoch] so the OLD MediaProjection callback queued by
     * `projection.stop()` and any late job fail their [MirrorSessionPolicy.isLive]
     * check instead of killing the replacement.
     */
    private fun tearDownCapture() {
        controlAllowed = false
        activeConnection = null
        captureEpoch++
        elevatedDisplayListener?.let {
            getSystemService(android.hardware.display.DisplayManager::class.java)?.unregisterDisplayListener(it)
        }
        elevatedDisplayListener = null
        elevatedSocket?.let { runCatching { it.close() } }
        elevatedSocket = null
        elevatedStarting = false
        encoder?.stop()
        encoder = null
        sound?.stop()
        sound = null
        projection?.let { runCatching { it.stop() } }
        projection = null
        pcId = null
        activeRequest = null
    }

    override fun onDestroy() {
        stopSharing()
        scope.cancel()
        super.onDestroy()
    }

    companion object {
        private const val TAG = "MirrorService"
        internal const val CHANNEL = "mirroring"
        private const val NOTIFICATION_ID = 4
        private const val ACTION_STOP = "app.nectarlink.android.mirror.STOP"
        private const val EXTRA_PC = "pc"
        private const val EXTRA_ELEVATED = "elevated"
        private const val EXTRA_CONSENT = "consent"
        private const val EXTRA_RESULT = "result"
        private const val EXTRA_MAX_SIZE = "maxSize"
        private const val EXTRA_FPS = "fps"
        private const val EXTRA_BITRATE = "bitrate"
        private const val EXTRA_AUDIO = "audio"
        private const val EXTRA_STAY_AWAKE = "stayAwake"
        private const val EXTRA_SCREEN_OFF = "screenOff"
        private const val EXTRA_REQUEST = "request"
        private const val EXTRA_CONNECTION = "connection"
        private const val EXTRA_ISSUE = "issue"

        /** The service sharing the screen now, if any. */
        @Volatile private var current: MirrorService? = null

        /** Stamps each start sent to the service, in sending order. */
        private val issued = AtomicLong()

        /** The last start issued before the user's latest Stop; see [MirrorSessionPolicy.admitsStart]. */
        @Volatile private var stoppedThrough = 0L

        private val _session = MutableStateFlow(MirrorSession())
        val session: StateFlow<MirrorSession> = _session.asStateFlow()

        fun createChannel(context: Context) {
            context.getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel(CHANNEL, context.getString(R.string.channel_mirroring), NotificationManager.IMPORTANCE_LOW),
            )
        }

        /** Starts sharing with `request`'s PC, with the user's consent (from the capture prompt). */
        fun start(context: Context, request: MirrorRequest, result: Int, consent: Intent, connection: ULong?) {
            val intent = Intent(context, MirrorService::class.java)
                .putExtra(EXTRA_PC, request.pcId)
                .putExtra(EXTRA_CONSENT, consent)
                .putExtra(EXTRA_RESULT, result)
                .putExtra(EXTRA_MAX_SIZE, request.maxSize)
                .putExtra(EXTRA_FPS, request.fps)
                .putExtra(EXTRA_BITRATE, request.bitrate)
                .putExtra(EXTRA_AUDIO, request.audio)
                .putExtra(EXTRA_STAY_AWAKE, request.stayAwake)
                .putExtra(EXTRA_SCREEN_OFF, request.screenOff)
                .putExtra(EXTRA_ISSUE, issued.incrementAndGet())
            if (request.request != null) intent.putExtra(EXTRA_REQUEST, request.request.toLong())
            if (connection != null) intent.putExtra(EXTRA_CONNECTION, connection.toLong())
            ContextCompat.startForegroundService(context, intent)
        }

        /**
         * Starts full-screen sharing with `request`'s PC immediately through
         * the Elevated shell helper, without a `MediaProjection` consent prompt.
         */
        fun startElevated(context: Context, request: MirrorRequest, connection: ULong): Boolean {
            if (!app.nectarlink.android.elevated.Elevated.running) return false
            val intent = Intent(context, MirrorService::class.java)
                .putExtra(EXTRA_PC, request.pcId)
                .putExtra(EXTRA_ELEVATED, true)
                .putExtra(EXTRA_MAX_SIZE, request.maxSize)
                .putExtra(EXTRA_FPS, request.fps)
                .putExtra(EXTRA_BITRATE, request.bitrate)
                .putExtra(EXTRA_AUDIO, request.audio)
                .putExtra(EXTRA_STAY_AWAKE, request.stayAwake)
                .putExtra(EXTRA_SCREEN_OFF, request.screenOff)
                .putExtra(EXTRA_ISSUE, issued.incrementAndGet())
            if (request.request != null) intent.putExtra(EXTRA_REQUEST, request.request.toLong())
            intent.putExtra(EXTRA_CONNECTION, connection.toLong())
            return runCatching {
                ContextCompat.startForegroundService(context, intent)
                true
            }.getOrDefault(false)
        }

        /** The PC stopped this exact request. Keep the token through posting. */
        fun stop(pcId: String, request: ULong? = null, connection: ULong? = null) {
            val service = current ?: return
            if (connection != null && service.activeConnection != connection) return
            val stoppedRequest = request ?: service.activeRequest
            val stoppedConnection = connection ?: service.activeConnection
            val stoppedEpoch = service.captureEpoch
            if (service.pcId == pcId && service.activeRequest == stoppedRequest && service.activeConnection == stoppedConnection) {
                service.controlAllowed = false
            }
            service.main.post {
                if (service.pcId == pcId && service.activeRequest == stoppedRequest && service.activeConnection == stoppedConnection && service.captureEpoch == stoppedEpoch) service.stopSharing()
            }
        }

        /** Only an actual live capture on the original peer/request/connection. */
        fun isControlTarget(pcId: String, context: app.nectarlink.core.PhoneControlContext): Boolean {
            val service = current ?: return false
            val epoch = service.captureEpoch
            return MirrorSessionPolicy.canControl(
                service.controlAllowed, service.pcId, service.activeRequest, service.activeConnection,
                pcId, context.request, context.connection,
            ) && service.controlAllowed && epoch == service.captureEpoch && current === service
        }

        /** Stops any active screen mirroring session from the UI. */
        fun stopCurrent() {
            val service = current ?: return
            // Deny posted input before this phone's Stop tap returns, not only
            // when the later main-thread capture teardown executes.
            service.controlAllowed = false
            service.main.post { service.stopByUser() }
        }

        /** The PC needs a keyframe. */
        fun keyframe(pcId: String) {
            val service = current?.takeIf { it.pcId == pcId } ?: return
            service.encoder?.requestKeyframe()
            service.requestElevatedKeyframe()
        }

        /** Updates keep-awake and screen-off state while mirroring to [pcId]. */
        fun setPower(pcId: String, stayAwake: Boolean, screenOff: Boolean) {
            val service = current ?: return
            service.main.post {
                if (service.pcId == pcId) {
                    service.stayAwake = stayAwake
                    service.screenOff = screenOff
                    _session.value = _session.value.copy(
                        stayAwake = stayAwake,
                        screenOff = screenOff,
                    )
                    service.applyPower()
                }
            }
        }
    }
}
