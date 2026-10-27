// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Window
import app.nectarlink

// A phone's screen, or one of its apps, in a window of its own, sized like
// the phone. One per mirroring (`mirrorKey`); closing it ends that one.
Window {
    id: win
    required property string mirrorKey
    // This window's mirroring (see Mirror.windows).
    readonly property var info: {
        try {
            return JSON.parse(Mirror.windows).find(w => w.key === mirrorKey) || ({})
        } catch (e) {
            return ({})
        }
    }
    readonly property string deviceName: info.name || ""
    readonly property string phase: info.phase || ""
    readonly property bool canControl: info.canControl === true
    readonly property bool viewOnly: info.viewOnly === true
    // Negotiated control state from the backend (request-bound phone-control
    // contract): available, ownerApproval, stopped, inputUnavailable,
    // unsupported, offline, notShowing, mirroringOff. Missing on old peers
    // or fixtures. The UI only reads it; grants and Stop live on the phone.
    readonly property string controlReason: info.controlReason || ""
    // Screen-reader description of the phone surface: the control state
    // plus the local focus keys. It only reads existing backend data
    // (phase/canControl/viewOnly/controlReason); the UI never grants
    // control itself. Reasons name this connection only, never the OS.
    readonly property string accessibleControlDescription: {
        if (win.phase !== "showing")
            return ""
        if (win.canControl) {
            return qsTr("Phone control is on. Type or use arrow keys to control the phone. Press F6 to reach the toolbar. Escape in the toolbar returns here.")
        }
        if (win.controlReason === "ownerApproval") {
            return qsTr("Phone control needs approval. Turn on Allow phone control for this computer in NectarLink's settings on the phone.")
        }
        if (win.controlReason === "stopped") {
            return qsTr("Phone control was stopped on the phone. To allow this computer again, turn Screen sharing and typing off and then on for this computer in NectarLink's settings on the phone.")
        }
        if (win.controlReason === "inputUnavailable") {
            return qsTr("To control the phone, turn on Control from your PC in NectarLink's settings on the phone.")
        }
        if (win.controlReason === "unsupported" || win.viewOnly) {
            return qsTr("Viewing only for this connection. Remote control isn't supported on this phone connection.")
        }
        return qsTr("Phone control isn't available for this connection right now.")
    }
    readonly property bool onMac: Qt.platform.os === "osx" || Qt.platform.os === "macos"
    readonly property bool fullScreen: visibility === Window.FullScreen
    property int visibilityBeforeFullScreen: Window.Windowed

    function toggleFullScreen() {
        if (fullScreen) {
            visibility = visibilityBeforeFullScreen
        } else {
            visibilityBeforeFullScreen = visibility === Window.Maximized ? Window.Maximized : Window.Windowed
            visibility = Window.FullScreen
        }
    }

    // Full screen from anywhere in this window, including toolbar buttons:
    // StandardKey.FullScreen is Ctrl+Meta+F (Command+Control+F) on macOS
    // and F11 on Windows/Linux.
    Shortcut {
        sequences: [StandardKey.FullScreen]
        enabled: win.phase === "showing"
        onActivated: win.toggleFullScreen()
    }

    visible: true
    title: info.app ? qsTr("%1 · %2").arg(info.title).arg(deviceName)
         : deviceName.length > 0 ? qsTr("%1 · NectarLink").arg(deviceName) : qsTr("Phone screen · NectarLink")
    color: "black"
    minimumWidth: 240
    minimumHeight: 240

    // A phone-shaped window the first time a picture arrives (portrait or
    // landscape), within the screen, or restored to where the user left it.
    readonly property size frame: video.frameSize
    property bool sized: false
    property bool readyForResize: false

    Timer {
        id: resizeTimer
        interval: 300
        repeat: false
        onTriggered: {
            if (!win.info.app || !win.sized)
                return
            if (!win.fullScreen)
                Mirror.saveGeometry(win.mirrorKey, win.x, win.y, win.width, win.height)
            if (win.phase === "showing")
                Mirror.resize(win.mirrorKey, win.width, win.height)
        }
    }

    Timer {
        id: moveTimer
        interval: 300
        repeat: false
        onTriggered: {
            if (win.info.app && win.sized && !win.fullScreen)
                Mirror.saveGeometry(win.mirrorKey, win.x, win.y, win.width, win.height)
        }
    }

    onWidthChanged: if (info.app && sized && readyForResize) resizeTimer.restart()
    onHeightChanged: if (info.app && sized && readyForResize) resizeTimer.restart()
    onXChanged: if (info.app && sized && readyForResize) moveTimer.restart()
    onYChanged: if (info.app && sized && readyForResize) moveTimer.restart()
    onPhaseChanged: if (phase === "showing" && info.app && sized) resizeTimer.restart()
    onActiveChanged: if (win.active) Mirror.syncClipboardOnFocus(win.mirrorKey)

    onFrameChanged: {
        if (frame.width <= 0)
            return
        const availW = Screen.desktopAvailableWidth > 0 ? Screen.desktopAvailableWidth : 1920
        const availH = Screen.desktopAvailableHeight > 0 ? Screen.desktopAvailableHeight : 1080
        if (!sized && info.app && (info.savedWidth || 0) >= 240 && (info.savedHeight || 0) >= 240) {
            readyForResize = false
            const w = Math.max(240, Math.min(Math.round(availW * 0.95), info.savedWidth))
            const h = Math.max(240, Math.min(Math.round(availH * 0.95), info.savedHeight))
            width = w
            height = h
            if (info.savedX !== null && info.savedX !== undefined && info.savedY !== null && info.savedY !== undefined) {
                x = Math.max(0, Math.min(availW - w, info.savedX))
                y = Math.max(0, Math.min(availH - h, info.savedY))
            }
            sized = true
            Qt.callLater(() => { win.readyForResize = true })
            return
        }
        const maxH = availH * 0.85
        const maxW = availW * 0.85
        const scale = Math.min(maxH / frame.height, maxW / frame.width, 1)
        if (!sized || (!info.app && !fullScreen && Math.abs(width / height - frame.width / frame.height) > 0.05)) {
            readyForResize = false
            width = Math.round(frame.width * scale)
            height = Math.round(frame.height * scale)
            sized = true
            if (info.app)
                Mirror.saveGeometry(mirrorKey, x, y, width, height)
            Qt.callLater(() => { win.readyForResize = true })
        }
    }
    width: 420
    height: 860
    onClosing: {
        if (info.app && sized && !fullScreen)
            Mirror.saveGeometry(mirrorKey, x, y, width, height)
        Mirror.stop(mirrorKey)
    }

    VideoView {
        id: video
        objectName: "mirrorVideo"
        anchors.fill: parent
        stream: win.mirrorKey
        windowIcon: win.info.icon || ""
        windowAppId: win.info.app && (win.info.pkg || "").length > 0
            ? "Nectarlink.App." + (win.info.device || "").slice(0, 8) + "." + win.info.pkg
            : ""
        focus: true
        // The phone picture for screen readers: the name follows the
        // window title (no new string), the description is the control
        // state, so revoked control is announced without implying it.
        Accessible.role: Accessible.Grouping
        Accessible.name: win.phase === "showing" ? win.title : ""
        Accessible.description: win.accessibleControlDescription
        acceptsText: win.phase === "showing" && win.canControl
        // The emoji panel (Win+.) and input methods give finished text here
        // instead of as key presses.
        onTextCommitted: (text) => Mirror.text(win.mirrorKey, text)

        // Where on the phone's screen a point in the window is (0 to 1).
        function at(x, y) {
            const r = pictureRect()
            return { x: (x - r.x) / Math.max(1, r.width), y: (y - r.y) / Math.max(1, r.height),
                     inside: x >= r.x && x <= r.x + r.width && y >= r.y && y <= r.y + r.height }
        }

        DropArea {
            id: mirrorDropArea
            anchors.fill: parent
            enabled: win.phase === "showing"
            onEntered: (drag) => {
                Mirror.syncClipboardOnFocus(win.mirrorKey)
            }
            onDropped: (drop) => {
                const urls = []
                if (drop.urls) {
                    for (let i = 0; i < drop.urls.length; i++) {
                        urls.push(drop.urls[i].toString())
                    }
                }
                if (urls.length > 0) {
                    Mirror.syncClipboardOnFocus(win.mirrorKey)
                    Mirror.dropFiles(urls)
                }
                drop.acceptProposedAction()
            }
        }

        HoverHandler {
            id: videoHover
            onHoveredChanged: {
                if (hovered) {
                    toolbarWakeTimer.restart()
                    if (win.phase === "showing")
                        Mirror.enterWindow(win.mirrorKey)
                } else {
                    Mirror.leaveWindow()
                }
            }
        }

        // The mouse: left is a finger, right is Back, middle is Home.
        MouseArea {
            anchors.fill: parent
            enabled: win.phase === "showing"
            acceptedButtons: Qt.LeftButton | Qt.RightButton | Qt.MiddleButton
            cursorShape: win.canControl ? Qt.PointingHandCursor : Qt.ArrowCursor
            property bool down: false
            property double lastMove: 0
            onPressed: (mouse) => {
                video.forceActiveFocus()
                if (!win.canControl) {
                    hintClose.closed = false
                    hintPulse.restart()
                    Mirror.requestControl(win.mirrorKey)
                    return
                }
                const p = video.at(mouse.x, mouse.y)
                if (mouse.button === Qt.RightButton) { Mirror.press(win.mirrorKey, "back"); return }
                if (mouse.button === Qt.MiddleButton) { Mirror.press(win.mirrorKey, "home"); return }
                if (!p.inside) return
                down = true
                Mirror.touch(win.mirrorKey, "down", p.x, p.y)
            }
            onPositionChanged: (mouse) => {
                if (!down || !win.canControl) return
                // About 60 a second is plenty for a finger.
                const now = Date.now()
                if (now - lastMove < 16) return
                lastMove = now
                const p = video.at(mouse.x, mouse.y)
                Mirror.touch(win.mirrorKey, "move", p.x, p.y)
            }
            onReleased: (mouse) => {
                if (!down) return
                down = false
                if (!win.canControl) return
                const p = video.at(mouse.x, mouse.y)
                Mirror.touch(win.mirrorKey, "up", p.x, p.y)
            }
            onWheel: (wheel) => {
                if (!win.canControl) {
                    hintClose.closed = false
                    Mirror.requestControl(win.mirrorKey)
                    return
                }
                const p = video.at(wheel.x, wheel.y)
                if (p.inside)
                    Mirror.scroll(win.mirrorKey, p.x, p.y, -wheel.angleDelta.x / 120, -wheel.angleDelta.y / 120)
            }
        }

        // The keyboard: shortcuts for screenshots and recording work even
        // without remote control; text and navigation keys go to the phone.
        // On macOS Qt reports Command as ControlModifier and the Control
        // key as MetaModifier. Full screen is a window-level Shortcut above
        // so it also fires from toolbar buttons.
        Keys.onPressed: (event) => {
            if (win.phase !== "showing")
                return
            // Local focus key, never sent to the phone (F6 has no phone
            // binding): Tab stays phone-bound while control is on, so it
            // cannot be the way out of the surface. Wakes the toolbar so
            // the keyboard focus ring is visible.
            if (event.key === Qt.Key_F6) {
                if (toolbar.visible) {
                    toolbarWakeTimer.restart()
                    copyShotBtn.forceActiveFocus(Qt.TabFocusReason)
                    event.accepted = true
                }
                return
            }
            if ((event.modifiers & Qt.ControlModifier) && (event.modifiers & Qt.ShiftModifier) && event.key === Qt.Key_C) {
                Mirror.screenshotClipboard(win.mirrorKey)
                event.accepted = true
                return
            }
            if ((event.modifiers & Qt.ControlModifier) && !(event.modifiers & Qt.ShiftModifier) && event.key === Qt.Key_S) {
                Mirror.screenshotFile(win.mirrorKey)
                event.accepted = true
                return
            }
            if ((event.modifiers & Qt.ControlModifier) && !(event.modifiers & Qt.ShiftModifier) && event.key === Qt.Key_R) {
                Mirror.toggleRecording(win.mirrorKey)
                event.accepted = true
                return
            }
            if (!win.canControl)
                return
            const keys = {}
            keys[Qt.Key_Escape] = "back"
            keys[Qt.Key_Back] = "back"
            keys[Qt.Key_Home] = "home"
            keys[Qt.Key_Return] = "enter"
            keys[Qt.Key_Enter] = "enter"
            keys[Qt.Key_Backspace] = "backspace"
            keys[Qt.Key_Delete] = "delete"
            keys[Qt.Key_Left] = "left"
            keys[Qt.Key_Right] = "right"
            keys[Qt.Key_Up] = "up"
            keys[Qt.Key_Down] = "down"
            keys[Qt.Key_Tab] = "tab"
            if ((event.modifiers & Qt.ControlModifier) && event.key === Qt.Key_V) {
                Mirror.paste(win.mirrorKey)
                event.accepted = true
            } else if (keys[event.key] !== undefined) {
                Mirror.press(win.mirrorKey, keys[event.key])
                event.accepted = true
            } else if (event.text.length > 0 && !(event.modifiers & (Qt.ControlModifier | Qt.AltModifier))
                       && event.text.charCodeAt(0) >= 32) {
                Mirror.text(win.mirrorKey, event.text)
                event.accepted = true
            }
        }
    }

    property int recordingElapsedSecs: 0
    function updateRecordingElapsed() {
        if (win.info.recording === true && (win.info.recordingStartedMs || 0) > 0) {
            win.recordingElapsedSecs = Math.max(0, Math.floor((Date.now() - win.info.recordingStartedMs) / 1000))
        } else {
            win.recordingElapsedSecs = 0
        }
    }
    onInfoChanged: updateRecordingElapsed()

    Timer {
        interval: 1000
        repeat: true
        running: win.info.recording === true
        onTriggered: win.updateRecordingElapsed()
    }

    // Back, Home and Recents are global actions on the phone, not routed to
    // an app's own display, so they are offered for the whole phone only.
    readonly property bool canNavigate: win.phase === "showing" && win.canControl
        && !win.info.app && win.mirrorKey.length > 0
    function pressNav(name) {
        if (win.canNavigate)
            Mirror.press(win.mirrorKey, name)
    }

    function formatDuration(secs) {
        const m = Math.floor(secs / 60)
        const s = secs % 60
        return (m < 10 ? "0" + m : "" + m) + ":" + (s < 10 ? "0" + s : "" + s)
    }

    // Floating mirror toolbar: screenshots, MP4 recording, stay awake, screen off, audio mute,
    // full screen, and the phone's Back/Home/Recents.
    Timer {
        id: toolbarWakeTimer
        interval: 1800
        repeat: false
    }
    Item {
        anchors.top: parent.top
        anchors.left: parent.left
        anchors.right: parent.right
        height: 56
        HoverHandler { id: topZoneHover }
    }
    Rectangle {
        id: toolbar
        objectName: "mirrorToolbar"
        anchors.top: parent.top
        anchors.right: parent.right
        anchors.margins: 12
        width: toolbarFlow.width + 8
        height: toolbarFlow.height + 6
        radius: 20
        color: Qt.rgba(0, 0, 0, 0.68)
        visible: win.phase === "showing" && video.frameSize.width > 0
        // Group label for screen readers; the buttons keep their own
        // labels. Escape/F6 here return focus to the phone surface and
        // are never sent to the phone (unaccepted keys propagate up from
        // the focused button to this container, never sideways to video).
        Accessible.role: Accessible.Grouping
        Accessible.name: qsTr("Mirror toolbar")
        Keys.onPressed: (event) => {
            if (event.key === Qt.Key_Escape || event.key === Qt.Key_F6) {
                video.forceActiveFocus()
                event.accepted = true
            }
        }
        readonly property bool toolbarHasFocus: copyShotBtn.activeFocus || saveShotBtn.activeFocus
            || recordBtn.activeFocus || stayAwakeBtn.activeFocus || screenOffBtn.activeFocus || soundBtn.activeFocus
            || fullScreenBtn.activeFocus || navBackBtn.activeFocus || navHomeBtn.activeFocus || navRecentsBtn.activeFocus
        opacity: topZoneHover.hovered || toolbarHover.hovered || toolbarWakeTimer.running
            || win.info.recording === true || win.info.screenOff === true || toolbarHasFocus ? 1 : 0
        Behavior on opacity { enabled: !Theme.reduceMotion; NumberAnimation { duration: Theme.fadeFast } }

        HoverHandler { id: toolbarHover }

        // Too narrow for one row (down to the 240 px minimum window), the
        // buttons wrap onto more rows at full size instead of running off
        // the window's left edge. The width is the widest row, worked out
        // from the same conditions that show each button rather than from
        // the Flow's own size or children, so it cannot loop.
        readonly property real flowWidth: {
            const b = copyShotBtn.implicitWidth
            const s = toolbarFlow.spacing
            const items = [b, b, b]
            if (win.info.recording === true)
                items.push(recordingTime.implicitWidth)
            items.push(b)
            if (win.info.canScreenOff === true)
                items.push(b)
            if (win.info.sound === true)
                items.push(b)
            items.push(b)
            if (!win.info.app)
                items.push(3 * b + 2 * s)
            const limit = Math.max(b, win.width - 2 * anchors.margins - 8)
            let widest = 0
            let x = 0
            for (const w of items) {
                if (x > 0 && x + w > limit) {
                    widest = Math.max(widest, x - s)
                    x = 0
                }
                x += w + s
            }
            return Math.max(widest, x - s)
        }

        Flow {
            id: toolbarFlow
            anchors.centerIn: parent
            width: toolbar.flowWidth
            spacing: 2

            IconButton {
                id: copyShotBtn
                objectName: "mirrorCopyShot"
                iconPath: Icons.copy
                iconColor: "white"
                label: qsTr("Copy screenshot (%1)").arg(win.onMac ? "⌘⇧C" : "Ctrl+Shift+C")
                onClicked: Mirror.screenshotClipboard(win.mirrorKey)
            }
            IconButton {
                id: saveShotBtn
                iconPath: Icons.camera
                iconColor: "white"
                label: qsTr("Save screenshot (%1)").arg(win.onMac ? "⌘S" : "Ctrl+S")
                onClicked: Mirror.screenshotFile(win.mirrorKey)
            }
            IconButton {
                id: recordBtn
                enabled: win.info.canRecord !== false
                iconPath: win.info.recording === true ? Icons.stopSquare : Icons.record
                iconColor: win.info.recording === true ? Theme.error : "white"
                label: (win.info.recording === true ? qsTr("Stop recording (%1)") : qsTr("Record video (%1)"))
                    .arg(win.onMac ? "⌘R" : "Ctrl+R")
                onClicked: Mirror.toggleRecording(win.mirrorKey)
            }
            Txt {
                id: recordingTime
                visible: win.info.recording === true
                height: copyShotBtn.implicitHeight
                leftPadding: 2
                rightPadding: 6
                role: "code"
                size: 12
                color: "white"
                text: win.formatDuration(win.recordingElapsedSecs)
            }
            IconButton {
                id: stayAwakeBtn
                iconPath: Icons.coffee
                iconColor: win.info.stayAwake === true ? Theme.primary : "white"
                label: win.info.stayAwake === true
                    ? qsTr("Phone stays awake while mirroring (turn off)")
                    : qsTr("Keep phone awake while mirroring")
                onClicked: Mirror.setStayAwake(win.mirrorKey, !(win.info.stayAwake === true))
            }
            IconButton {
                id: screenOffBtn
                visible: win.info.canScreenOff === true
                iconPath: Icons.screenOff
                iconColor: win.info.screenOff === true ? Theme.primary : "white"
                label: win.info.screenOff === true
                    ? qsTr("Turn phone screen back on")
                    : qsTr("Turn phone screen off while mirroring")
                onClicked: Mirror.setScreenOff(win.mirrorKey, !(win.info.screenOff === true))
            }
            IconButton {
                id: soundBtn
                visible: win.info.sound === true
                iconPath: Mirror.muted ? Icons.soundOff : Icons.speaker
                iconColor: "white"
                label: Mirror.muted ? qsTr("Play the phone's sound here") : qsTr("Mute the phone's sound here")
                onClicked: Mirror.toggleSound()
            }
            IconButton {
                id: fullScreenBtn
                iconPath: win.fullScreen
                    ? "M9 3v6H3M15 3v6h6M9 21v-6H3M15 21v-6h6"
                    : "M3 9V3h6M21 9V3h-6M3 15v6h6M21 15v6h-6"
                iconColor: "white"
                label: (win.fullScreen ? qsTr("Exit full screen (%1)") : qsTr("Full screen (%1)"))
                    .arg(win.onMac ? "⌃⌘F" : "F11")
                onClicked: win.toggleFullScreen()
            }
            // One Flow item, so Back/Home/Recents wrap together.
            Row {
                id: navRow
                visible: !win.info.app
                spacing: toolbarFlow.spacing
                // Without control the buttons are dimmed and say why to
                // screen readers; a click brings back the banner that says
                // why, and nothing goes to the phone.
                TapHandler {
                    enabled: !win.canControl
                    onTapped: {
                        hintClose.closed = false
                        hintPulse.restart()
                    }
                }
                IconButton {
                    id: navBackBtn
                    objectName: "mirrorNavBack"
                    enabled: win.canNavigate
                    iconPath: Icons.back
                    iconColor: "white"
                    label: qsTr("Back")
                    Accessible.description: win.canControl ? "" : win.accessibleControlDescription
                    onClicked: win.pressNav("back")
                }
                IconButton {
                    id: navHomeBtn
                    objectName: "mirrorNavHome"
                    enabled: win.canNavigate
                    iconPath: Icons.home
                    iconColor: "white"
                    label: qsTr("Home")
                    Accessible.description: win.canControl ? "" : win.accessibleControlDescription
                    onClicked: win.pressNav("home")
                }
                IconButton {
                    id: navRecentsBtn
                    objectName: "mirrorNavRecents"
                    enabled: win.canNavigate
                    iconPath: Icons.history
                    iconColor: "white"
                    label: qsTr("Recent apps")
                    Accessible.description: win.canControl ? "" : win.accessibleControlDescription
                    onClicked: win.pressNav("recents")
                }
            }
        }
    }

    // The phone's sound is muted (in every window): a small badge in the
    // toolbar's corner says so, instead of the whole toolbar staying over
    // the picture. A click plays the sound again.
    Rectangle {
        anchors.top: parent.top
        anchors.right: parent.right
        anchors.margins: 12
        width: 32
        height: 32
        radius: 16
        color: Qt.rgba(0, 0, 0, 0.68)
        visible: opacity > 0
        opacity: Mirror.muted && win.info.sound === true && toolbar.visible && toolbar.opacity === 0 ? 1 : 0
        Behavior on opacity { enabled: !Theme.reduceMotion; NumberAnimation { duration: Theme.fadeFast } }
        IconButton {
            anchors.centerIn: parent
            width: 28
            height: 28
            iconSize: 14
            iconPath: Icons.soundOff
            iconColor: "white"
            label: qsTr("Play the phone's sound here")
            onClicked: Mirror.toggleSound()
        }
    }

    // Brief confirmation pill for screenshots, recordings, file drops, and power toggles.
    Rectangle {
        id: noticePill
        Accessible.role: Accessible.StatusBar
        Accessible.name: noticePill.pillText
        readonly property string pillText: mirrorDropArea.containsDrag
            ? qsTr("Release to send files to %1").arg(win.deviceName.length > 0 ? win.deviceName : qsTr("phone"))
            : (win.info.notice || "")
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.bottom: hintBanner.visible ? hintBanner.top : parent.bottom
        anchors.bottomMargin: hintBanner.visible ? 8 : 16
        width: Math.min(parent.width - 24, noticeTxt.implicitWidth + 24)
        height: noticeTxt.implicitHeight + 12
        radius: height / 2
        color: Qt.rgba(0, 0, 0, 0.78)
        visible: opacity > 0
        opacity: pillText.length > 0 ? 1 : 0
        Behavior on opacity { enabled: !Theme.reduceMotion; NumberAnimation { duration: Theme.fadeFast } }
        Txt {
            id: noticeTxt
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.leftMargin: 12
            anchors.rightMargin: 12
            anchors.verticalCenter: parent.verticalCenter
            horizontalAlignment: Text.AlignHCenter
            elide: Text.ElideRight
            role: "caption"
            color: "white"
            text: noticePill.pillText
        }
    }

    // Why control is unavailable for this connection, and what (if anything)
    // the phone owner can do about it. Text mirrors
    // accessibleControlDescription so the banner and screen reader agree.
    Rectangle {
        id: hintBanner
        Accessible.role: Accessible.StatusBar
        Accessible.name: hint.text
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        anchors.margins: 12
        height: Math.max(hint.implicitHeight + 20, 48)
        radius: Theme.radiusMd
        visible: win.phase === "showing" && video.frameSize.width > 0 && !win.canControl && !win.info.app && !hintClose.closed
        color: Qt.rgba(0, 0, 0, 0.72)

        SequentialAnimation {
            id: hintPulse
            NumberAnimation {
                target: hintBanner
                property: "scale"
                from: 1.0
                to: Theme.reduceMotion ? 1.0 : 1.03
                duration: 110
                easing.type: Easing.OutQuad
            }
            NumberAnimation {
                target: hintBanner
                property: "scale"
                from: Theme.reduceMotion ? 1.0 : 1.03
                to: 1.0
                duration: 140
                easing.type: Easing.InOutQuad
            }
        }

        Txt {
            id: hint
            anchors.left: parent.left
            anchors.right: notifyPhoneBtn.left
            anchors.margins: 10
            anchors.verticalCenter: parent.verticalCenter
            wrapMode: Text.WordWrap
            role: "bodySmall"
            color: "white"
            text: win.accessibleControlDescription
        }
        Button {
            id: notifyPhoneBtn
            anchors.right: hintClose.left
            anchors.rightMargin: 4
            anchors.verticalCenter: parent.verticalCenter
            variant: "tonal"
            size: "sm"
            text: qsTr("How to enable")
            enabled: !win.viewOnly
            onClicked: Mirror.requestControl(win.mirrorKey)
        }
        IconButton {
            id: hintClose
            property bool closed: false
            anchors.right: parent.right
            anchors.rightMargin: 4
            anchors.verticalCenter: parent.verticalCenter
            iconPath: Icons.close
            iconColor: "white"
            label: qsTr("Hide")
            onClicked: closed = true
        }
    }

    // Before the picture: waiting for the phone, or why it ended.
    Rectangle {
        anchors.fill: parent
        visible: win.phase !== "showing" || video.frameSize.width <= 0
        color: Theme.surface
        Column {
            anchors.centerIn: parent
            width: Math.min(parent.width - 48, 320)
            spacing: 14
            Spinner {
                anchors.horizontalCenter: parent.horizontalCenter
                visible: win.phase !== "ended"
                width: 28; height: 28
            }
            RoundedImage {
                anchors.horizontalCenter: parent.horizontalCenter
                visible: win.phase === "ended" && win.info.app === true && (win.info.icon || "").length > 0
                width: 40; height: 40
                radius: 10
                source: win.info.icon || ""
            }
            Icon {
                anchors.horizontalCenter: parent.horizontalCenter
                visible: win.phase === "ended" && !(win.info.app === true && (win.info.icon || "").length > 0)
                width: 32; height: 32
                path: Icons.mirror
                color: Theme.surfaceContentVariant
            }
            Txt {
                width: parent.width
                horizontalAlignment: Text.AlignHCenter
                wrapMode: Text.WordWrap
                role: "title"
                text: win.phase === "asking" ? (win.info.app ? qsTr("Opening %1").arg(win.info.title) : qsTr("Allow it on your phone"))
                    : win.phase === "ended" ? (win.info.app ? qsTr("%1 closed").arg(win.info.title) : qsTr("Mirroring stopped"))
                    : qsTr("Starting…")
            }
            Txt {
                width: parent.width
                horizontalAlignment: Text.AlignHCenter
                wrapMode: Text.WordWrap
                role: "body"
                muted: true
                text: win.phase === "asking" && !win.info.app
                      ? qsTr("Tap the notification on %1, then choose to share the entire screen.").arg(win.deviceName)
                      : win.phase === "ended" ? ((win.info.reason || "").length > 0 ? win.info.reason
                                                : win.info.app ? qsTr("The app closed on %1.").arg(win.deviceName)
                                                : qsTr("The phone stopped sharing its screen."))
                      : ""
            }
            Button {
                anchors.horizontalCenter: parent.horizontalCenter
                visible: win.phase === "ended"
                variant: "tonal"
                text: win.info.app ? qsTr("Open again") : qsTr("Try again")
                onClicked: Mirror.reopen(win.mirrorKey)
            }
        }
    }
}
