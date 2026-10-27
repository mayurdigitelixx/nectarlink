// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.mirror

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.ComponentName
import android.content.Context
import android.graphics.Path
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.Settings
import android.util.Log
import android.view.WindowManager
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import app.nectarlink.android.NectarlinkApplication
import app.nectarlink.core.MirrorInputEvent
import app.nectarlink.core.TouchPhase
import java.util.concurrent.atomic.AtomicLong
import kotlin.math.abs
import kotlin.math.hypot

/**
 * The PC's mouse and keyboard on this phone while its screen is mirrored
 * (Assist level, docs/protocol/mirror.md): an accessibility service, which
 * the user turns on once. Clicks become taps, holds long presses, drags
 * swipes (played when the button comes up: Android plays a gesture whole),
 * the wheel scrolls, and typing goes into the focused text field.
 */
class InputService : AccessibilityService() {
    private val main = Handler(Looper.getMainLooper())

    /** The finger's way so far, as fractions of the screen, and when it went down. */
    private val stroke = mutableListOf<Pair<Float, Float>>()
    private var downAt = 0L
    private var longPressFired = false
    private var inputOwner: Any? = null
    private var inputCurrent: () -> Boolean = { false }
    private var inputGeneration = 0L
    private val interruptFence = InputInterruptFence()

    private val longPressRunnable = Runnable {
        if (!inputCurrent()) { clearPendingInput(); return@Runnable }
        if (stroke.isEmpty() || longPressFired) return@Runnable
        val (w, h) = screen()
        val points = stroke.map { (px, py) -> px * w to py * h }
        if (isStationaryTapOrHold(points, w)) {
            longPressFired = true
            val start = points.first()
            gesture(Path().apply { moveTo(start.first, start.second) }, LONG_PRESS_DURATION_MS)
        }
    }

    // Wheel notches are gathered briefly into one swipe and queued while a gesture is in flight.
    private var scrollX = 0f
    private var scrollY = 0f
    private var scrollDx = 0f
    private var scrollDy = 0f
    private var gestureInFlight = false
    private val flushScroll = Runnable { flushScroll() }
    private var clearGestureInFlight: Runnable? = null

    override fun onServiceConnected() {
        instance = this
        (application as NectarlinkApplication).core.refreshNotificationAccess()
    }

    override fun onUnbind(intent: android.content.Intent?): Boolean {
        clearPendingInput()
        main.removeCallbacksAndMessages(null)
        gestureInFlight = false
        instance = null
        (application as NectarlinkApplication).core.refreshNotificationAccess()
        return super.onUnbind(intent)
    }

    override fun onDestroy() {
        clearPendingInput()
        main.removeCallbacksAndMessages(null)
        gestureInFlight = false
        if (instance === this) instance = null
        super.onDestroy()
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}

    override fun onInterrupt() {
        interruptFence.interrupt()
        clearPendingInput()
    }

    private fun handle(input: MirrorInputEvent) {
        if (!inputCurrent()) { clearPendingInput(); return }
        when (input) {
            is MirrorInputEvent.Touch -> touch(input.action, input.x, input.y)
            is MirrorInputEvent.Scroll -> scroll(input.x, input.y, input.dx, input.dy)
            is MirrorInputEvent.Key -> key(input.key)
            is MirrorInputEvent.Text -> type(input.text)
            is MirrorInputEvent.Gamepad -> {
                app.nectarlink.android.elevated.Elevated.sendGamepad(
                    input.buttons.toInt(),
                    input.lx.toInt(),
                    input.ly.toInt(),
                    input.rx.toInt(),
                    input.ry.toInt(),
                    input.lt.toInt(),
                    input.rt.toInt(),
                )
            }
        }
    }

    // ---- Touch ----

    private fun touch(action: TouchPhase, x: Float, y: Float) {
        when (action) {
            TouchPhase.DOWN -> {
                main.removeCallbacks(longPressRunnable)
                stroke.clear()
                stroke.add(x to y)
                downAt = SystemClock.uptimeMillis()
                longPressFired = false
                main.postDelayed(longPressRunnable, LONG_PRESS_MS)
            }
            TouchPhase.MOVE -> if (stroke.isNotEmpty()) {
                stroke.add(x to y)
                if (!longPressFired) {
                    val (w, h) = screen()
                    val start = stroke.first().let { it.first * w to it.second * h }
                    if (hypot(x * w - start.first, y * h - start.second) >= TAP_SLOP_FRACTION * w) {
                        main.removeCallbacks(longPressRunnable)
                    }
                }
            }
            TouchPhase.UP -> {
                main.removeCallbacks(longPressRunnable)
                if (stroke.isEmpty()) return
                stroke.add(x to y)
                val held = SystemClock.uptimeMillis() - downAt
                val (w, h) = screen()
                val points = stroke.map { (px, py) -> px * w to py * h }
                stroke.clear()
                val wasLongPressFired = longPressFired
                longPressFired = false
                val start = points.first()
                if (isStationaryTapOrHold(points, w)) {
                    if (wasLongPressFired) return
                    val path = Path().apply { moveTo(start.first, start.second) }
                    val duration = if (held >= LONG_PRESS_MS) LONG_PRESS_DURATION_MS else TAP_MS
                    gesture(path, duration)
                } else {
                    val filtered = filterDragPoints(points)
                    val path = Path().apply {
                        moveTo(filtered.first().first, filtered.first().second)
                        filtered.drop(1).forEach { (px, py) -> lineTo(px, py) }
                    }
                    gesture(path, swipeDurationMs(held))
                }
            }
        }
    }

    private fun scroll(x: Float, y: Float, dx: Float, dy: Float) {
        scrollX = x
        scrollY = y
        scrollDx += dx
        scrollDy += dy
        main.removeCallbacks(flushScroll)
        if (!gestureInFlight) {
            main.postDelayed(flushScroll, SCROLL_GATHER_MS)
        }
    }

    private fun flushScroll() {
        main.removeCallbacks(flushScroll)
        if (!inputCurrent()) { clearPendingInput(); return }
        if (gestureInFlight) return
        val (w, h) = screen()
        val x = scrollX * w
        val y = scrollY * h
        // Wheel down shows what's below: the finger moves up.
        val dx = (-scrollDx * SCROLL_STEP_FRACTION * w).coerceIn(-x + 1, w - x - 1)
        val dy = (-scrollDy * SCROLL_STEP_FRACTION * h).coerceIn(-y + 1, h - y - 1)
        scrollDx = 0f
        scrollDy = 0f
        if (abs(dx) < 1f && abs(dy) < 1f) return
        gesture(Path().apply { moveTo(x, y); lineTo(x + dx, y + dy) }, SCROLL_MS)
    }

    private fun gesture(path: Path, durationMs: Long) {
        val current = inputCurrent
        val owner = inputOwner
        val generation = inputGeneration
        if (!current()) { clearPendingInput(); return }
        val clampedDuration = durationMs.coerceAtLeast(1L)
        val description = GestureDescription.Builder()
            .addStroke(GestureDescription.StrokeDescription(path, 0, clampedDuration))
            .build()
        gestureInFlight = true
        clearGestureInFlight?.let { main.removeCallbacks(it) }
        val timeout = Runnable { onGestureFinished(generation, owner, current) }
        clearGestureInFlight = timeout
        main.postDelayed(timeout, maxOf(GESTURE_TIMEOUT_MS, clampedDuration + 150L))
        // Preserve the original owner even through a gathered scroll/long press.
        if (!current()) { clearPendingInput(); return }
        val dispatched = dispatchGesture(
            description,
            object : GestureResultCallback() {
                override fun onCompleted(gestureDescription: GestureDescription?) {
                    onGestureFinished(generation, owner, current)
                }

                override fun onCancelled(gestureDescription: GestureDescription?) {
                    onGestureFinished(generation, owner, current)
                }
            },
            main,
        )
        if (!dispatched) {
            Log.i(TAG, "a gesture wasn't played")
            onGestureFinished(generation, owner, current)
        }
    }

    private fun onGestureFinished(generation: Long, owner: Any?, current: () -> Boolean) {
        if (inputGeneration != generation || inputOwner != owner) return
        clearGestureInFlight?.let { main.removeCallbacks(it) }
        clearGestureInFlight = null
        gestureInFlight = false
        if (!current()) { clearPendingInput(); return }
        if (scrollDx != 0f || scrollDy != 0f) flushScroll()
    }

    /** Retire only this service's queued input; already dispatched OS work
     * cannot be recalled. A stale completion cannot flush the next owner's scroll. */
    private fun clearPendingInput() {
        inputGeneration++
        main.removeCallbacks(longPressRunnable)
        main.removeCallbacks(flushScroll)
        clearGestureInFlight?.let { main.removeCallbacks(it) }
        clearGestureInFlight = null
        stroke.clear()
        scrollDx = 0f
        scrollDy = 0f
        longPressFired = false
        gestureInFlight = false
        pendingText = null
        inputOwner = null
        inputCurrent = { false }
    }

    /** The screen's size in pixels, as it's turned now. */
    private fun screen(): Pair<Float, Float> {
        val windows = getSystemService(WindowManager::class.java)
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            windows.maximumWindowMetrics.bounds.let { it.width().toFloat() to it.height().toFloat() }
        } else {
            val metrics = android.util.DisplayMetrics()
            @Suppress("DEPRECATION")
            windows.defaultDisplay.getRealMetrics(metrics)
            metrics.widthPixels.toFloat() to metrics.heightPixels.toFloat()
        }
    }

    // ---- Keys and text ----

    private fun key(key: String) {
        if (!inputCurrent()) { clearPendingInput(); return }
        when (key) {
            "back" -> performGlobalAction(GLOBAL_ACTION_BACK)
            "home" -> performGlobalAction(GLOBAL_ACTION_HOME)
            "recents" -> performGlobalAction(GLOBAL_ACTION_RECENTS)
            "notifications" -> performGlobalAction(GLOBAL_ACTION_NOTIFICATIONS)
            "enter" -> focused()?.let { node ->
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R && inputCurrent()) {
                    node.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.id)
                }
            }
            "backspace" -> edit { text, start, end ->
                if (start != end) Edit(text.removeRange(start, end), start)
                else if (start > 0) {
                    val prev = text.offsetByCodePoints(start, -1)
                    Edit(text.removeRange(prev, start), prev)
                } else null
            }
            "delete" -> edit { text, start, end ->
                if (start != end) Edit(text.removeRange(start, end), start)
                else if (end < text.length) {
                    val next = text.offsetByCodePoints(end, 1)
                    Edit(text.removeRange(end, next), start)
                } else null
            }
            "left" -> edit { text, start, _ -> Edit(text, if (start > 0) text.offsetByCodePoints(start, -1) else 0) }
            "right" -> edit { text, _, end -> Edit(text, if (end < text.length) text.offsetByCodePoints(end, 1) else text.length) }
            "up" -> edit { text, start, _ ->
                val prevNewline = text.lastIndexOf('\n', (start - 1).coerceAtLeast(0))
                Edit(text, if (prevNewline >= 0) prevNewline else 0)
            }
            "down" -> edit { text, _, end ->
                val nextNewline = text.indexOf('\n', end)
                Edit(text, if (nextNewline >= 0) (nextNewline + 1).coerceAtMost(text.length) else text.length)
            }
            "tab" -> focused()?.focusSearch(android.view.View.FOCUS_FORWARD)?.let { node ->
                if (inputCurrent()) node.performAction(AccessibilityNodeInfo.ACTION_FOCUS)
            }
        }
    }

    private fun type(typed: String) = edit { text, start, end -> Edit(text.replaceRange(start, end, typed), start + typed.length) }

    private data class Edit(val text: String, val cursor: Int)

    private var pendingText: String? = null
    private var pendingCursor: Int = 0
    private var pendingAt: Long = 0L

    /** Changes the focused text field: its text and selection in, the new text and cursor out. */
    private fun edit(change: (String, Int, Int) -> Edit?) {
        val node = focused() ?: return
        if (!isNodeEditable(node)) return
        runCatching { node.refresh() }
        val showingHint = node.isShowingHintText
        val rawText = if (showingHint) "" else node.text?.toString().orEmpty()
        val now = SystemClock.uptimeMillis()
        val cachedPending = pendingText
        val usePending = cachedPending != null && (now - pendingAt) < 350L && rawText != cachedPending
        val text = if (usePending) cachedPending.orEmpty() else rawText
        val start = if (usePending || (rawText == cachedPending && node.textSelectionStart <= 0 && pendingCursor > 0)) {
            pendingCursor.coerceIn(0, text.length)
        } else {
            node.textSelectionStart.takeIf { it in 0..text.length } ?: text.length
        }
        val end = if (usePending || (rawText == cachedPending && node.textSelectionEnd <= 0 && pendingCursor > 0)) {
            start
        } else {
            node.textSelectionEnd.takeIf { it in start..text.length } ?: start
        }
        val result = change(text, start, end) ?: return
        if (!inputCurrent()) { clearPendingInput(); return }
        pendingText = result.text
        pendingCursor = result.cursor
        pendingAt = now
        if (result.text != text || usePending) {
            node.performAction(
                AccessibilityNodeInfo.ACTION_SET_TEXT,
                Bundle().apply { putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, result.text) },
            )
        }
        if (!inputCurrent()) { clearPendingInput(); return }
        node.performAction(
            AccessibilityNodeInfo.ACTION_SET_SELECTION,
            Bundle().apply {
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, result.cursor)
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, result.cursor)
            },
        )
    }

    private fun isNodeEditable(node: AccessibilityNodeInfo): Boolean =
        node.isEditable ||
            node.className == "android.widget.EditText" ||
            node.actionList.any { it.id == AccessibilityNodeInfo.ACTION_SET_TEXT }

    private fun focused(): AccessibilityNodeInfo? {
        findFocus(AccessibilityNodeInfo.FOCUS_INPUT)?.takeIf { isNodeEditable(it) }?.let { return it }
        findFocus(AccessibilityNodeInfo.FOCUS_ACCESSIBILITY)?.takeIf { isNodeEditable(it) }?.let { return it }
        val roots = buildList {
            rootInActiveWindow?.let { add(it) }
            runCatching { windows }.getOrNull()?.forEach { w -> w.root?.let { add(it) } }
        }
        for (root in roots) {
            findEditable(root, requireFocused = true)?.let { return it }
        }
        for (root in roots) {
            findEditable(root, requireFocused = false)?.let { return it }
        }
        return findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
    }

    private fun findEditable(node: AccessibilityNodeInfo?, requireFocused: Boolean): AccessibilityNodeInfo? {
        if (node == null) return null
        if (isNodeEditable(node) && (!requireFocused || node.isFocused)) return node
        for (i in 0 until node.childCount) {
            findEditable(node.getChild(i), requireFocused)?.let { return it }
        }
        return null
    }

    companion object {
        private const val TAG = "InputService"
        internal const val TAP_MS = 40L
        internal const val LONG_PRESS_MS = 520L
        internal const val LONG_PRESS_DURATION_MS = 550L
        internal const val SWIPE_MIN_MS = 80L
        internal const val SWIPE_MAX_MS = 600L
        internal const val GESTURE_TIMEOUT_MS = 700L
        internal const val TAP_SLOP_FRACTION = 0.015f
        internal const val DRAG_MIN_STEP_PX = 1.5f
        internal const val SCROLL_STEP_FRACTION = 0.12f
        internal const val SCROLL_MS = 75L
        internal const val SCROLL_GATHER_MS = 24L

        @Volatile private var instance: InputService? = null

        /** Whether the user turned it on (and Android has it running). */
        val running: Boolean get() = instance != null

        /**
         * The PC's input; ignored while the service is off, and dropped on the main thread
         * unless [stillCurrent] holds right before it's performed.
         */
        fun handle(input: MirrorInputEvent, stillCurrent: () -> Boolean, owner: Any? = null) {
            val service = instance ?: return
            val interruptGeneration = service.interruptFence.snapshot()
            service.main.post {
                if (!service.interruptFence.isCurrent(interruptGeneration)) return@post
                if (instance !== service || !stillCurrent()) return@post
                if (service.inputOwner != owner || !service.inputCurrent()) service.clearPendingInput()
                service.inputOwner = owner
                service.inputCurrent = { instance === service && stillCurrent() }
                service.handle(input)
            }
        }

        /** Whether it's turned on in Settings (it may not be running yet). */
        fun enabled(context: Context): Boolean {
            val enabled = Settings.Secure.getString(context.contentResolver, Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES).orEmpty()
            val me = ComponentName(context, InputService::class.java)
            return enabled.split(':').any { ComponentName.unflattenFromString(it) == me }
        }

        /** Android's accessibility settings, where the user turns it on. */
        fun settingsIntent(): android.content.Intent =
            android.content.Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS).addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK)
    }
}

internal fun maxTravelledFromStart(points: List<Pair<Float, Float>>): Float {
    val start = points.firstOrNull() ?: return 0f
    var maxDist = 0f
    for (i in 1 until points.size) {
        val pt = points[i]
        val d = hypot(pt.first - start.first, pt.second - start.second)
        if (d > maxDist) maxDist = d
    }
    return maxDist
}

internal fun isStationaryTapOrHold(points: List<Pair<Float, Float>>, screenWidthPx: Float): Boolean =
    maxTravelledFromStart(points) < InputService.TAP_SLOP_FRACTION * screenWidthPx

internal fun filterDragPoints(
    points: List<Pair<Float, Float>>,
    minStepPx: Float = InputService.DRAG_MIN_STEP_PX,
): List<Pair<Float, Float>> {
    if (points.isEmpty()) return emptyList()
    val result = ArrayList<Pair<Float, Float>>(points.size)
    var prev = points.first()
    result.add(prev)
    for (i in 1 until points.size) {
        val pt = points[i]
        if (hypot(pt.first - prev.first, pt.second - prev.second) >= minStepPx) {
            result.add(pt)
            prev = pt
        }
    }
    return result
}

internal fun swipeDurationMs(heldMs: Long): Long =
    heldMs.coerceIn(InputService.SWIPE_MIN_MS, InputService.SWIPE_MAX_MS)


/** Reject handler work queued before Accessibility interrupted this service. */
internal class InputInterruptFence {
    private val generation = AtomicLong(0L)

    fun snapshot(): Long = generation.get()

    fun interrupt() {
        generation.incrementAndGet()
    }

    fun isCurrent(snapshot: Long): Boolean = generation.get() == snapshot
}
