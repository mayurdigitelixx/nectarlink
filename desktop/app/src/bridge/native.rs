// SPDX-License-Identifier: GPL-3.0-or-later
//! Qt calls that cxx-qt-lib doesn't wrap (see `cpp/app_helpers.cpp`).

#[cxx::bridge]
pub mod ffi {
    unsafe extern "C++" {
        include!("app_helpers.h");
        include!("video_view.h");
        include!("cxx-qt-lib/qqmlapplicationengine.h");
        type QQmlApplicationEngine = cxx_qt_lib::QQmlApplicationEngine;

        /// The latest picture of a video stream (32-bit BGRX rows), for
        /// `VideoView`s showing it. Any thread.
        fn video_frame(stream: &str, width: u32, height: u32, bgrx: &[u8]);
        /// The stream ended: its views go blank. Any thread.
        fn video_clear(stream: &str);

        /// Must run before `QGuiApplication` is created.
        fn prepare_qt();
        /// Like `prepare_qt`, without the QML log file: for the installer
        /// and uninstaller, which mustn't create the app's data folder.
        fn prepare_qt_without_log();
        /// Like `prepare_qt`, for `--ui-check`: Qt's messages go to stderr
        /// instead of the log file, and QML warnings and errors are counted.
        fn prepare_qt_for_ui_check();
        /// How many QML warnings and errors there were since
        /// `prepare_qt_for_ui_check`.
        fn qml_problem_count() -> i32;
        fn add_app_icon_image(size: i32, rgba: &[u8]);
        fn apply_app_icon();
        fn keep_running_without_windows();
        /// Registers the fonts compiled in under `:/fonts/`; returns how
        /// many loaded.
        fn load_bundled_fonts() -> i32;
        /// Clears Qt pixmap caches and trims the process working set.
        fn trim_memory_caches();
        /// Records the active QML engine so `trim_memory_caches` can run GC and
        /// clear component caches.
        fn register_qml_app_engine(engine: Pin<&mut QQmlApplicationEngine>);
        /// Starts a native OS drag-and-drop operation for a local file or text.
        fn start_external_drag(file_path: &str, text_fallback: &str) -> bool;

        /// Installs the NectarLink and Qt translations for `language` (a
        /// locale name, or "" for the system's), looking in `dir` for the
        /// .qm files and replacing those installed before. Run after the
        /// `QGuiApplication` exists; before any QML engine loads, or later to
        /// retranslate the open windows. Returns true when NectarLink's own
        /// translation was installed.
        fn install_translator(dir: &str, language: &str) -> bool;

        /// `text` in the installed language (`QCoreApplication::translate`).
        fn translate(context: &str, text: &str) -> String;
    }
}
