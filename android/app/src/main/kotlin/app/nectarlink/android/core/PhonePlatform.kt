// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.core

import android.app.ActivityOptions
import android.app.PendingIntent
import android.app.RemoteInput
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Bundle
import app.nectarlink.android.calls.PhoneCalls
import app.nectarlink.android.camera.ContinuityCameraRequests
import app.nectarlink.android.clipboard.PhoneClipboard
import app.nectarlink.android.contacts.PhoneContacts
import app.nectarlink.android.notifications.NotificationListener
import app.nectarlink.android.media.PhoneMedia
import app.nectarlink.android.photos.RecentPhotos
import app.nectarlink.android.elevated.Elevated
import app.nectarlink.android.mirror.AppWindows
import app.nectarlink.android.mirror.InputService
import app.nectarlink.android.mirror.MirrorRequest
import app.nectarlink.android.mirror.MirrorRequests
import app.nectarlink.android.mirror.MirrorService
import app.nectarlink.android.sms.PhoneSms
import app.nectarlink.android.storage.PhoneStorage
import app.nectarlink.android.toggles.PhoneToggles
import app.nectarlink.android.webcam.WebcamRequest
import app.nectarlink.android.webcam.WebcamRequests
import app.nectarlink.android.webcam.WebcamService
import app.nectarlink.core.CallCommand
import app.nectarlink.core.CallLogEntry
import app.nectarlink.core.Contact
import app.nectarlink.core.SmsAttachment
import app.nectarlink.core.SmsMessage
import app.nectarlink.core.SmsPartData
import app.nectarlink.core.SmsThread
import app.nectarlink.core.FileToSend
import app.nectarlink.core.MediaAction
import app.nectarlink.core.MirrorInputEvent
import app.nectarlink.core.MirrorOptions
import app.nectarlink.core.PhoneApp
import app.nectarlink.core.PhoneToggleValue
import app.nectarlink.core.PhotoAlbum
import app.nectarlink.core.PhotoItem
import app.nectarlink.core.PhotoThumb
import app.nectarlink.core.NotificationFailure
import app.nectarlink.core.Platform
import app.nectarlink.core.StorageEntry
import app.nectarlink.core.StorageReadFile
import app.nectarlink.core.StorageWriteDone
import app.nectarlink.core.TaskNotify
import app.nectarlink.core.WebcamOptions

/**
 * What the core asks of the phone: ring it, and act on its notifications
 * for a PC (dismiss, reply, run an action).
 */
internal class PhonePlatform(
    context: Context,
    private val ringer: Ringer,
    private val media: PhoneMedia,
    /** Shows a link a PC sent: (PC's ID, link) → shown. */
    private val onLink: (String, String) -> Boolean,
    /** Shows a PC's Play on phone offer: (PC's ID, player, title) → shown. */
    private val onWatch: (String, String, String?, app.nectarlink.core.WatchFileOffer?) -> Boolean,
    /** Asks the user to start the microphone for a PC: PC's ID → asked. */
    private val onMic: (String) -> Boolean,
    /** The PC stopped asking for (or listening to) the microphone: PC's ID. */
    private val onMicStop: (String) -> Unit,
    /** Shows or updates a watched task from a PC: (PC's ID, task) → shown. */
    private val onTask: (String, TaskNotify) -> Boolean,
    /** The phone's calls (created after this). */
    private val calls: () -> PhoneCalls,
    /** The phone's contacts (created after this). */
    private val contacts: () -> PhoneContacts,
    /** The phone's texts (created after this). */
    private val sms: () -> PhoneSms,
    /** The phone's quick settings (created after this). */
    private val toggles: () -> PhoneToggles,
    /** The phone's shared storage (created after this). */
    private val storage: () -> PhoneStorage,
    /** Asks the user to share the screen with a PC: (PC's ID, request) → asked. */
    private val onMirror: (String, MirrorRequest, ULong) -> Boolean,
    /** Asks the user to stream the camera as a webcam to a PC: (PC's ID, request) → asked. */
    private val onWebcam: (String, WebcamRequest) -> Boolean,
    /** Apps in windows of their own on PCs (Elevated). */
    private val appWindows: AppWindows,
    private val legacyAppWindows: app.nectarlink.android.mirror.LegacyAppWindows,
    /** Resolves a paired PC's display name by ID. */
    private val nameOf: (String) -> String = { "" },
    /** Notifies when a PC starts or stops typing on this phone without mirroring. */
    private val onKeyboardFromPc: (String, Boolean) -> Unit = { _, _ -> },
    /** Whether a PC's screen input admitted under a typing generation may still reach the OS. */
    private val typingCurrent: (String, ULong) -> Boolean = { _, _ -> false },
    /** Required real Node-bound guard retained across any posted input. */
    private val controlCurrent: (String, app.nectarlink.core.PhoneControlContext) -> (() -> Boolean),
) : Platform {
    private val context = context.applicationContext

    override fun startRinging() = ringer.startRinging()

    override fun stopRinging() = ringer.stopRinging()

    override fun setClipboard(text: String): Boolean = PhoneClipboard.write(context, text)

    override fun setClipboardRich(text: String, html: String?, rtf: String?): Boolean =
        PhoneClipboard.write(context, text, html)

    override fun setClipboardImage(mime: String, bytes: ByteArray): Boolean =
        PhoneClipboard.writeImage(context, mime, bytes)

    override fun mediaCommand(player: String, action: MediaAction, position: ULong?) =
        media.command(player, action, position)

    override fun openLink(fromId: String, url: String): Boolean = onLink(fromId, url)

    override fun watchOffered(
        fromId: String,
        player: String,
        title: String?,
        file: app.nectarlink.core.WatchFileOffer?,
    ): Boolean = onWatch(fromId, player, title, file)

    override fun taskNotify(fromId: String, task: TaskNotify): Boolean = onTask(fromId, task)

    override fun openPhoto(id: String): FileToSend? = RecentPhotos.open(context, id)

    override fun photoAlbums(): List<PhotoAlbum> = RecentPhotos.albums(context)

    override fun photoList(album: String?, before: Long?, beforeId: String, limit: UInt): List<PhotoItem> =
        RecentPhotos.list(context, album, before, beforeId, limit.toInt())

    override fun photoThumbs(ids: List<String>): List<PhotoThumb> = RecentPhotos.thumbs(context, ids)

    override fun callCommand(id: String, command: CallCommand): Boolean = calls().command(id, command)

    override fun callLog(before: Long?, limit: UInt): List<CallLogEntry> =
        calls().callLog(before, limit.toInt())

    override fun callDial(number: String): Boolean = calls().dial(number)

    override fun contacts(query: String?, offset: UInt, limit: UInt): List<Contact> =
        contacts().list(query, offset.toInt(), limit.toInt())

    override fun mirrorRequested(pcId: String, options: MirrorOptions, connection: ULong, capture: ULong): Boolean =
        if (options.app != null) {
            appWindows.open(pcId, options, connection, capture)
        } else {
            val request = MirrorRequest(
                pcId,
                options.maxSize.toInt(),
                options.fps.toInt(),
                options.bitrate.toInt(),
                options.audio,
                options.stayAwake,
                options.screenOff,
                options.request,
            )
            if (Elevated.running && MirrorService.startElevated(context, request, connection)) {
                true
            } else {
                onMirror(pcId, request, connection)
            }
        }

    override fun mirrorAppRequestedLegacy(pcId: String, options: MirrorOptions, connection: ULong): Boolean =
        options.app != null && options.session != SCREEN && legacyAppWindows.open(pcId, options, connection)

    override fun mirrorAppStopRequestedLegacy(pcId: String, session: UInt, connection: ULong) {
        if (session != SCREEN) legacyAppWindows.close(pcId, session, connection)
    }

    override fun mirrorAppInputLegacy(pcId: String, session: UInt, input: MirrorInputEvent, connection: ULong, generation: ULong) {
        if (session != SCREEN) legacyAppWindows.input(pcId, session, input, connection, generation)
    }

    override fun mirrorPowerRequested(pcId: String, stayAwake: Boolean, screenOff: Boolean) {
        MirrorService.setPower(pcId, stayAwake, screenOff)
        appWindows.setPower(pcId, stayAwake, screenOff)
        legacyAppWindows.setPower(pcId, stayAwake, screenOff)
    }

    override fun mirrorStopRequested(pcId: String, session: UInt) {
        // App captures can only be stopped by their exact owned callback.
        if (session != SCREEN) return
        onKeyboardFromPc(pcId, false)
        MirrorRequests.dismiss(context, pcId)
        MirrorService.stop(pcId)
    }

    override fun mirrorStopRequestedOwned(pcId: String, session: UInt, request: ULong?, connection: ULong, capture: ULong) {
        if (session != SCREEN) {
            if (request != null) appWindows.close(pcId, session, request, connection, capture)
            return
        }
        if (request == null) return mirrorStopRequested(pcId, session)
        // Core removed the exact incoming request already. Pending consent may
        // still finish, but the service's accepted-owner check rejects that start.
        MirrorService.stop(pcId, request, connection)
    }

    override fun mirrorKeyframeRequested(pcId: String, session: UInt) {
        if (session == SCREEN) MirrorService.keyframe(pcId) else {
            appWindows.keyframe(pcId, session)
            legacyAppWindows.keyframe(pcId, session)
        }
    }

    override fun mirrorResizeRequested(pcId: String, session: UInt, width: UInt, height: UInt) {
        if (session != SCREEN) legacyAppWindows.resize(pcId, session, width.toInt(), height.toInt())
    }

    override fun mirrorResizeRequestedOwned(pcId: String, width: UInt, height: UInt, context: app.nectarlink.core.PhoneControlContext) {
        if (context.session != SCREEN) appWindows.resize(pcId, context, width.toInt(), height.toInt())
    }

    // The screen's input comes through mirrorScreenInput, with its typing generation.
    override fun mirrorInput(pcId: String, session: UInt, input: MirrorInputEvent) {
        // Legacy app input has its own connection/epoch callback above.
        if (session != SCREEN) return
    }

    // Real events when Elevated runs; gestures through the accessibility service otherwise.
    // The generation is checked again right before the OS sees the input: a Stop on this
    // phone drops what's already on its way.
    override fun mirrorScreenInput(pcId: String, input: MirrorInputEvent, generation: ULong) {
        val current = { typingCurrent(pcId, generation) }
        if (!current()) return
        if (input is MirrorInputEvent.Key) {
            when (input.key) {
                "keyboard_on" -> {
                    onKeyboardFromPc(pcId, true)
                    return
                }
                "keyboard_off" -> {
                    onKeyboardFromPc(pcId, false)
                    return
                }
            }
        }
        if (input is MirrorInputEvent.Text && input.text.any { it.code > 127 } && InputService.running) {
            InputService.handle(input, current)
            return
        }
        if (!current()) return
        if (!Elevated.handle(input, current)) InputService.handle(input, current, pcId to generation)
    }

    override fun mirrorControlInput(pcId: String, input: MirrorInputEvent, context: app.nectarlink.core.PhoneControlContext) {
        val admitted = controlCurrent(pcId, context)
        if (context.session != SCREEN) {
            appWindows.input(pcId, context, input, admitted)
            return
        }
        val current = { admitted() && MirrorService.isControlTarget(pcId, context) }
        if (!current()) return
        // Remote keyboard mode and gamepad are outside this screen-control contract.
        if (input is MirrorInputEvent.Gamepad) return
        if (input is MirrorInputEvent.Key && input.key in listOf("keyboard_on", "keyboard_off")) return
        if (input is MirrorInputEvent.Text && input.text.any { it.code > 127 } && InputService.running) {
            InputService.handle(input, current, pcId to context)
            return
        }
        if (!current()) return
        if (!Elevated.handle(input, current)) InputService.handle(input, current, pcId to context)
    }

    override fun webcamRequested(pcId: String, options: WebcamOptions): Boolean {
        val request = WebcamRequest(
            pcId = pcId,
            width = options.width.toInt(),
            height = options.height.toInt(),
            fps = options.fps.toInt(),
            bitrate = options.bitrate.toInt(),
            camera = options.camera,
        )
        return onWebcam(pcId, request)
    }

    override fun micRequested(pcId: String): Boolean = onMic(pcId)

    override fun micStopRequested(pcId: String) {
        onMicStop(pcId)
        app.nectarlink.android.mic.MicService.stop(context)
    }

    override fun webcamStopRequested(pcId: String) {
        WebcamRequests.dismiss(context, pcId)
        WebcamService.stopForPc(context, pcId)
    }

    override fun webcamKeyframeRequested(pcId: String) =
        WebcamService.keyframe(pcId)

    override fun cameraCaptureRequested(pcId: String, requestId: String, mode: String): Boolean =
        ContinuityCameraRequests.show(context, pcId, requestId, mode, nameOf(pcId))

    override fun cameraCaptureCancelled(pcId: String, requestId: String) {
        ContinuityCameraRequests.cancelFromPc(context, pcId, requestId)
    }

    override fun phoneApps(): List<PhoneApp> = AppWindows.list(context)

    override fun smsThreads(limit: UInt): List<SmsThread> = sms().threads(limit.toInt())

    override fun smsMessages(thread: String, before: Long?, limit: UInt): List<SmsMessage> =
        sms().messages(thread, before, limit.toInt())

    override fun smsSend(to: List<String>, body: String): Boolean = sms().send(to, body)

    override fun sendSmsWithAttachments(to: List<String>, body: String, attachments: List<SmsAttachment>): Boolean =
        sms().send(to, body, attachments)

    override fun smsPart(id: String): SmsPartData? = sms().part(id)

    override fun setPhoneToggle(id: String, value: PhoneToggleValue): Boolean =
        toggles().set(id, value)

    override fun storageList(path: String): List<StorageEntry> = storage().list(path)

    override fun storageOpenRead(path: String): StorageReadFile = storage().openRead(path)

    override fun storageWrite(path: String, stagedPath: String, modified: Long?): StorageWriteDone =
        storage().write(path, stagedPath, modified)

    override fun storageMkdir(path: String) = storage().mkdir(path)

    override fun storageRename(from: String, to: String) = storage().rename(from, to)

    override fun storageDelete(path: String, confirmed: Boolean) = storage().delete(path, confirmed)

    override fun dismissNotification(key: String) {
        val listener = NotificationListener.instance ?: throw NotificationFailure.Unsupported()
        listener.dismiss(key)
    }

    override fun runNotificationAction(key: String, action: String, reply: String?) {
        val listener = NotificationListener.instance ?: throw NotificationFailure.Unsupported()
        val sbn = listener.find(key) ?: throw NotificationFailure.NotFound()
        val target = action.toIntOrNull()?.let { sbn.notification.actions?.getOrNull(it) }
            ?: throw NotificationFailure.NotFound()
        val intent = target.actionIntent ?: throw NotificationFailure.NotFound()

        // A reply goes to the app the way the notification shade sends it:
        // the text as the result of the action's free-form input.
        val fillIn = Intent()
        if (reply != null) {
            val inputs = target.remoteInputs.orEmpty().filter { it.allowFreeFormInput }
            if (inputs.isEmpty()) throw NotificationFailure.NotFound()
            val results = Bundle().apply { inputs.forEach { putCharSequence(it.resultKey, reply) } }
            RemoteInput.addResultsToIntent(inputs.toTypedArray(), fillIn, results)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                RemoteInput.setResultsSource(fillIn, RemoteInput.SOURCE_FREE_FORM_INPUT)
            }
        }
        try {
            intent.send(context, 0, fillIn, null, null, null, sendOptions())
        } catch (e: PendingIntent.CanceledException) {
            throw NotificationFailure.NotFound()
        }
    }

    /** Lets an action open its app's screen, as tapping it on the phone would. */
    private fun sendOptions(): Bundle? {
        val mode = when {
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.BAKLAVA -> ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOW_ALWAYS
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE -> {
                @Suppress("DEPRECATION")
                ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOWED
            }
            else -> return null
        }
        return ActivityOptions.makeBasic().setPendingIntentBackgroundActivityStartMode(mode).toBundle()
    }

    private companion object {
        /** The phone's own screen's mirroring session. */
        const val SCREEN = 0u
    }
}
