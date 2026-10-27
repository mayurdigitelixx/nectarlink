// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.mirror

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import android.view.WindowManager
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import app.nectarlink.android.R
import app.nectarlink.android.elevated.AppDisplay
import app.nectarlink.android.elevated.Elevated
import app.nectarlink.core.MirrorInputEvent
import app.nectarlink.core.MirrorOptions
import app.nectarlink.core.MirrorSendResult
import app.nectarlink.core.MirrorStream
import app.nectarlink.core.VideoPacketKind
import app.nectarlink.core.mirrorConfig
import java.io.DataInputStream
import java.io.EOFException
import java.net.Socket
import java.util.concurrent.ConcurrentHashMap
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlin.math.roundToInt

/**
 * The upstream 1bd6955c app producer, used only before ownership negotiation.
 * Its real Node/connection guard never becomes an owned request context.
 * Phone apps in windows of their own on a PC (Elevated): each runs on a
 * display the shell-side helper makes for it, whose video goes to the PC
 * as a mirroring session of its own. A notification says so while any are
 * open, with a button to close them.
 */
class LegacyAppWindows(
    context: Context,
    private val scope: CoroutineScope,
    /** Retains the actual original Node; never constructs a PhoneControlContext. */
    private val owner: (String, ULong) -> app.nectarlink.core.NectarlinkNode?,
    private val currentNode: (app.nectarlink.core.NectarlinkNode) -> Boolean,
    /** A PC's name. */
    private val nameOf: (String) -> String,
) {
    private val context = context.applicationContext

    private class Window(
        val pc: String,
        val session: UInt,
        val socket: Socket,
        @Volatile var width: Int,
        @Volatile var height: Int,
        val dpi: Int,
        val maxSize: Int,
        val connection: ULong,
        val node: app.nectarlink.core.NectarlinkNode,
        val current: () -> Boolean,
    ) {
        @Volatile var display: Int = -1
    }

    private val windows = ConcurrentHashMap<Pair<String, UInt>, Window>()

    /** What each PC asks of the phone while its windows are open: (stay awake, screen off). */
    private val power = ConcurrentHashMap<String, Pair<Boolean, Boolean>>()

    init {
        instance = this
    }

    /** Runs the PC's app on a display of its own and streams it; false if it can't. */
    fun open(pc: String, options: MirrorOptions, connection: ULong): Boolean {
        val pkg = options.app ?: return false
        if (pkg == context.packageName || options.session == 0u) return false
        val node = owner(pc, connection) ?: return false
        val admitted = { currentNode(node) && node.mirrorLegacyAppCurrent(pc, options.session, connection) }
        if (!admitted()) return false
        val maxSize = options.maxSize.toInt().coerceIn(320, 1920)
        val (width, height, dpi) = size(maxSize)
        val key = pc to options.session
        var registered: Window? = null
        val starting = { admitted() && (registered?.let { windows[key] === it } ?: true) }
        val socket = Elevated.openApp(
            pkg, width, height, dpi, options.bitrate.toInt(), options.fps.toInt(),
            stillCurrent = starting,
            register = { socket ->
                val window = Window(pc, options.session, socket, width, height, dpi, maxSize, connection, node, admitted)
                val previous = synchronized(windows) {
                    if (!admitted()) return@openApp false
                    registered = window
                    windows.put(key, window)
                }
                previous?.let { runCatching { it.socket.close() } }
                // A concurrent Stop/replacement may have retired this exact socket.
                current(window)
            },
        )
        if (socket == null) {
            registered?.let(::close)
            return false
        }
        val window = registered ?: run { socket.close(); return false }
        if (!current(window)) {
            close(window)
            return false
        }
        power[pc] = options.stayAwake to options.screenOff
        changed()
        scope.launch(Dispatchers.IO) {
            if (!current(window)) { close(window); return@launch }
            val stream = runCatching { node.mirrorOpen(pc) }.getOrElse {
                Log.w(TAG, "can't stream an app window", it)
                close(window)
                return@launch
            }
            if (!current(window)) { stream.end(); close(window); return@launch }
            Thread({ forward(window, stream) }, "legacy-app-window").start()
        }
        return true
    }

    /** The helper's packets to the PC, until either side closes. */
    private fun forward(window: Window, stream: MirrorStream) {
        try {
            val input = DataInputStream(window.socket.getInputStream().buffered(1 shl 16))
            while (current(window)) {
                val length = input.readInt()
                if (length < 9 || length > MAX_PACKET) return
                val kind = input.readUnsignedByte()
                val time = input.readLong().toULong()
                val data = ByteArray(length - 9).also { input.readFully(it) }
                val result = when (kind) {
                    AppDisplay.KIND_CONFIG -> {
                        val fields = java.nio.ByteBuffer.wrap(data)
                        val (w, h) = fields.int to fields.int
                        window.width = w
                        window.height = h
                        window.display = fields.int
                        stream.send(VideoPacketKind.CONFIG, 0u, mirrorConfig(w.toUInt(), h.toUInt(), window.session, null))
                    }
                    AppDisplay.KIND_KEYFRAME -> stream.send(VideoPacketKind.KEYFRAME, time, data)
                    else -> stream.send(VideoPacketKind.FRAME, time, data)
                }
                when (result) {
                    MirrorSendResult.CLOSED -> return
                    MirrorSendResult.NEED_KEYFRAME, MirrorSendResult.DROPPED -> keyframe(window)
                    MirrorSendResult.QUEUED -> {}
                }
            }
        } catch (_: EOFException) {
        } catch (e: java.io.IOException) {
            Log.d(TAG, "an app window ended", e)
        } finally {
            stream.end()
            close(window)
        }
    }

    /** The PC closed a window. */
    fun close(pc: String, session: UInt, connection: ULong) {
        windows[pc to session]?.takeIf { it.connection == connection }?.let(::close)
    }

    private fun current(window: Window): Boolean =
        windows[window.pc to window.session] === window && window.current()

    private fun close(window: Window) {
        runCatching { window.socket.close() }
        if (windows.remove(window.pc to window.session, window)) changed()
    }

    fun closeAll() {
        windows.values.toList().forEach(::close)
    }

    fun keyframe(pc: String, session: UInt) {
        windows[pc to session]?.let(::keyframe)
    }

    private fun keyframe(window: Window) {
        if (!current(window)) return
        runCatching {
            synchronized(window) {
                if (!current(window)) return
                window.socket.getOutputStream().apply {
                    write("K\n".toByteArray())
                    flush()
                }
            }
        }
    }

    /** The PC resized an app window: fit to encoder limits and tell the helper. */
    fun resize(pc: String, session: UInt, width: Int, height: Int) {
        val window = windows[pc to session]?.takeIf(::current) ?: return
        val (phoneW, phoneH) = screen()
        val (newW, newH) = WindowShape.fit(width, height, window.maxSize, phoneW, phoneH)
        if (newW == window.width && newH == window.height) return
        window.width = newW
        window.height = newH
        runCatching {
            synchronized(window) {
                if (!current(window)) return
                window.socket.getOutputStream().apply {
                    write("R $newW $newH ${window.dpi}\n".toByteArray())
                    flush()
                }
            }
        }
    }

    /** The PC's mouse and keyboard on a window. */
    fun input(pc: String, session: UInt, event: MirrorInputEvent, connection: ULong, generation: ULong) {
        val window = windows[pc to session]?.takeIf(::current) ?: return
        if (window.display < 0 || window.connection != connection) return
        val admitted = {
            current(window) && window.node.mirrorLegacyAppInputCurrent(pc, session, connection, generation)
        }
        Elevated.handleOn(window.display, window.width.toFloat(), window.height.toFloat(), event, admitted)
    }

    /** The phone's screen, in pixels. */
    private fun screen(): Pair<Int, Int> {
        val windows = context.getSystemService(WindowManager::class.java)
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            windows.maximumWindowMetrics.bounds.let { it.width() to it.height() }
        } else {
            val metrics = android.util.DisplayMetrics()
            @Suppress("DEPRECATION")
            windows.defaultDisplay.getRealMetrics(metrics)
            metrics.widthPixels to metrics.heightPixels
        }
    }

    /** The phone's screen shape scaled to fit [maxSize] (see [WindowShape]), upright, and its density likewise. */
    private fun size(maxSize: Int): Triple<Int, Int, Int> {
        val (w, h) = screen()
        val (shortSide, longSide) = minOf(w, h) to maxOf(w, h)
        val (fitW, fitH) = WindowShape.fit(shortSide, longSide, maxSize.coerceAtLeast(320), w, h)
        val scale = fitH.toFloat() / longSide
        val dpi = (context.resources.displayMetrics.densityDpi * scale).roundToInt().coerceIn(120, 640)
        return Triple(fitW, fitH, dpi)
    }

    /** The PC turned keeping the phone awake, or its screen off, on or off. */
    fun setPower(pc: String, stayAwake: Boolean, screenOff: Boolean) {
        if (windows.values.none { it.pc == pc }) return
        power[pc] = stayAwake to screenOff
        applyPower()
    }

    private fun applyPower() {
        val open = windows.values.map { it.pc }.toSet()
        power.keys.retainAll(open)
        val wants = power.values
        StreamPower.set(context, "legacy_apps", open.isNotEmpty(), wants.any { it.first }, wants.any { it.second })
    }

    private fun changed() {
        applyPower()
        val manager = NotificationManagerCompat.from(context)
        if (windows.isEmpty()) {
            manager.cancel(NOTIFICATION_TAG, NOTIFICATION_ID)
            return
        }
        val allowed = Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
            context.checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
        if (!allowed) return
        MirrorService.createChannel(context)
        val pcs = windows.values.map { it.pc }.distinct().map(nameOf).filter { it.isNotEmpty() }
        val closeAll = PendingIntent.getBroadcast(
            context, 0, Intent(context, AppWindows.CloseReceiver::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notification = NotificationCompat.Builder(context, MirrorService.CHANNEL)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(
                context.resources.getQuantityString(R.plurals.app_windows_title, windows.size, windows.size),
            )
            .setContentText(
                context.getString(R.string.app_windows_text, pcs.joinToString().ifEmpty { context.getString(R.string.your_pc) }),
            )
            .setOngoing(true)
            .setSilent(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .addAction(0, context.getString(R.string.action_close_all), closeAll)
            .build()
        try {
            manager.notify(NOTIFICATION_TAG, NOTIFICATION_ID, notification)
        } catch (e: SecurityException) {
            Log.i(TAG, "can't say the app windows are open", e)
        }
    }

    companion object {
        private const val TAG = "LegacyAppWindows"
        private const val NOTIFICATION_TAG = "legacy_app_windows"
        private const val NOTIFICATION_ID = 7
        private const val MAX_PACKET = 8 shl 20
        @Volatile private var instance: LegacyAppWindows? = null
        fun closeCurrent() { instance?.closeAll() }
    }
}
