// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.mirror

import android.content.Context
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.Display
import app.nectarlink.core.MirrorSendResult
import app.nectarlink.core.MirrorStream
import app.nectarlink.core.VideoPacketKind
import app.nectarlink.core.mirrorConfigCodec
import java.util.concurrent.atomic.AtomicLong

/**
 * The screen as H.264 / H.265 / AV1 for a PC (docs/protocol/mirror.md): the projection
 * draws into the hardware encoder's input surface, and each encoded
 * picture goes straight to the stream. When the network is behind, the
 * stream drops pictures and this asks the encoder for a keyframe. Turning
 * the phone restarts the encoder at the new size.
 */
internal class ScreenEncoder(
    context: Context,
    private val projection: MediaProjection,
    private val stream: MirrorStream,
    private val maxSize: Int,
    private val fps: Int,
    private val bitrate: Int,
    preferredCodec: String = "h264",
    /** The answered request's generation, echoed in the stream's config (`null` predates correlation). */
    private val request: ULong? = null,
    private val onEnded: () -> Unit,
) {
    private val context = context.applicationContext
    private val displays = context.getSystemService(DisplayManager::class.java)
    private val main = Handler(Looper.getMainLooper())
    private val codecName = selectCodec(preferredCodec)

    @Volatile private var running = true
    @Volatile private var keyframeWanted = false
    /** Bumped on every default-display change; the encoder rechecks the size whenever it moves. */
    private val displayChanges = AtomicLong()
    /** API34+ projection callbacks describe the captured content, already in its current orientation. */
    @Volatile private var capturedContent: Pair<Int, Int>? = null

    private data class Size(val width: Int, val height: Int, val dpi: Int)

    private val rotation = object : DisplayManager.DisplayListener {
        override fun onDisplayChanged(displayId: Int) {
            if (displayId == Display.DEFAULT_DISPLAY) displayChanges.incrementAndGet()
        }
        override fun onDisplayAdded(displayId: Int) {}
        override fun onDisplayRemoved(displayId: Int) {}
    }

    fun start() {
        displays.registerDisplayListener(rotation, main)
        Thread(::run, "screen-encoder").start()
    }

    /** Called on main only after MirrorService admits this exact projection/capture epoch. */
    fun capturedContentResized(width: Int, height: Int) {
        if (!running || width <= 0 || height <= 0) return
        capturedContent = width to height
        displayChanges.incrementAndGet()
    }

    /** Stops (the encoder thread ends and calls `onEnded`). */
    fun stop() {
        running = false
    }

    /** The PC needs a fresh start. */
    fun requestKeyframe() {
        keyframeWanted = true
    }

    private fun run() {
        var display: VirtualDisplay? = null
        try {
            while (running) {
                // Read before the size, so a change landing after the read is seen by drain.
                val seen = displayChanges.get()
                val size = screenSize()
                val (activeCodecName, codec) = encoder(size)
                val surface = codec.createInputSurface()
                codec.start()
                val current = display
                if (current == null) {
                    display = projection.createVirtualDisplay(
                        "Nectarlink", size.width, size.height, size.dpi,
                        DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, surface, null, null,
                    )
                } else {
                    current.resize(size.width, size.height, size.dpi)
                    current.surface = surface
                }
                val closed = stream.send(
                    VideoPacketKind.CONFIG,
                    0u,
                    mirrorConfigCodec(activeCodecName, size.width.toUInt(), size.height.toUInt(), 0u, request),
                ) == MirrorSendResult.CLOSED || drain(codec, size, seen)
                runCatching { codec.stop() }
                codec.release()
                surface.release()
                if (closed) break
            }
        } catch (e: Exception) {
            Log.w(TAG, "screen sharing failed", e)
        } finally {
            running = false
            main.post { displays.unregisterDisplayListener(rotation) }
            display?.release()
            stream.end()
            onEnded()
        }
    }

    /** Sends what the encoder makes until the size changes (false) or it's over (true). */
    private fun drain(codec: MediaCodec, size: Size, seen: Long): Boolean {
        val info = MediaCodec.BufferInfo()
        // The parameter sets, sent again with each keyframe so the PC can
        // start from any of them.
        var parameters = ByteArray(0)
        var checked = seen
        while (running) {
            val changes = displayChanges.get()
            if (changes != checked) {
                checked = changes
                val now = screenSize()
                if (now != size) return false
            }
            if (keyframeWanted) {
                keyframeWanted = false
                codec.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
            }
            val index = codec.dequeueOutputBuffer(info, WAIT_US)
            if (index < 0) continue
            val bytes = ByteArray(info.size)
            codec.getOutputBuffer(index)?.let { buffer ->
                buffer.position(info.offset)
                buffer.get(bytes)
            }
            codec.releaseOutputBuffer(index, false)
            if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) return true
            if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                parameters = bytes
                continue
            }
            if (bytes.isEmpty()) continue
            val key = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
            val result = stream.send(
                if (key) VideoPacketKind.KEYFRAME else VideoPacketKind.FRAME,
                info.presentationTimeUs.toULong(),
                if (key) parameters + bytes else bytes,
            )
            when (result) {
                // A video stream never drops without asking for a keyframe.
                MirrorSendResult.NEED_KEYFRAME, MirrorSendResult.DROPPED -> keyframeWanted = true
                MirrorSendResult.CLOSED -> return true
                MirrorSendResult.QUEUED -> {}
            }
        }
        return !running
    }

    private fun createCodecForMime(mime: String, size: Size): MediaCodec {
        val format = MediaFormat.createVideoFormat(mime, size.width, size.height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            // Keyframes when the PC asks; a periodic one heals the rest.
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 10)
            // A still screen still sends now and then, so the PC isn't stale.
            setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 100_000)
            // Real time, no reordering.
            setInteger(MediaFormat.KEY_PRIORITY, 0)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) setInteger(MediaFormat.KEY_LATENCY, 1)
            // What the PC assumes when it turns the picture into RGB.
            setInteger(MediaFormat.KEY_COLOR_STANDARD, MediaFormat.COLOR_STANDARD_BT709)
            setInteger(MediaFormat.KEY_COLOR_RANGE, MediaFormat.COLOR_RANGE_LIMITED)
            setInteger(MediaFormat.KEY_COLOR_TRANSFER, MediaFormat.COLOR_TRANSFER_SDR_VIDEO)
        }
        val codec = MediaCodec.createEncoderByType(mime)
        codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        return codec
    }

    private fun encoder(size: Size): Pair<String, MediaCodec> {
        val preferredMime = mimeForCodec(codecName)
        if (preferredMime != MediaFormat.MIMETYPE_VIDEO_AVC) {
            runCatching { return codecName to createCodecForMime(preferredMime, size) }
        }
        return "h264" to createCodecForMime(MediaFormat.MIMETYPE_VIDEO_AVC, size)
    }

    /** The screen's size scaled to fit `maxSize`, in multiples of 16 (what encoders like). */
    private fun screenSize(): Size {
        val captured = capturedContent
        val (w, h, dpi) = captured?.let {
            fitCapturedContent(it.first, it.second, maxSize, liveDisplayMetrics(context).densityDpi)
        } ?: computeScreenSize(context, maxSize)
        return Size(w, h, dpi)
    }

    companion object {
        private const val TAG = "ScreenEncoder"
        private const val WAIT_US = 50_000L

        fun mimeForCodec(name: String): String = when (name.lowercase()) {
            "h265", "hevc" -> MediaFormat.MIMETYPE_VIDEO_HEVC
            "av1" -> if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) "video/av01" else MediaFormat.MIMETYPE_VIDEO_AVC
            else -> MediaFormat.MIMETYPE_VIDEO_AVC
        }

        fun selectCodec(preferred: String = "h264"): String {
            val want = preferred.lowercase()
            if (want != "h265" && want != "av1") return "h264"
            val mime = mimeForCodec(want)
            val supported = runCatching {
                android.media.MediaCodecList(android.media.MediaCodecList.REGULAR_CODECS)
                    .codecInfos
                    .any { info ->
                        info.isEncoder && info.supportedTypes.any { it.equals(mime, ignoreCase = true) }
                    }
            }.getOrDefault(false)
            return if (supported) want else "h264"
        }

        fun computeScreenSize(context: Context, maxSize: Int): Triple<Int, Int, Int> {
            // Query the live logical display instead of an application's cached
            // WindowManager configuration, which can retain the cover-screen shape.
            val metrics = liveDisplayMetrics(context)
            return fitCapturedContent(metrics.widthPixels, metrics.heightPixels, maxSize, metrics.densityDpi)
                ?: error("screen has no usable dimensions")
        }

        private fun liveDisplayMetrics(context: Context): android.util.DisplayMetrics {
            val display = context.getSystemService(DisplayManager::class.java)
                .getDisplay(Display.DEFAULT_DISPLAY)
            val metrics = android.util.DisplayMetrics()
            if (display != null) {
                @Suppress("DEPRECATION")
                display.getRealMetrics(metrics)
            }
            if (metrics.widthPixels <= 0 || metrics.heightPixels <= 0 || metrics.densityDpi <= 0) {
                metrics.setTo(context.resources.displayMetrics)
            }
            return metrics
        }
    }
}
