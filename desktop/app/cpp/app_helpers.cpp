// SPDX-License-Identifier: GPL-3.0-or-later
#include "app_helpers.h"
#include "video_view.h"

#include <QtCore/QCoreApplication>
#include <QtCore/QDir>
#include <QtCore/QFileInfo>
#include <QtCore/QLibraryInfo>
#include <QtCore/QLocale>
#include <QtCore/QMimeData>
#include <QtCore/QPointer>
#include <QtCore/QStringList>
#include <QtCore/QTranslator>
#include <QtCore/QUrl>
#include <QtCore/QtEnvironmentVariables>
#include <QtGui/QDrag>
#include <QtGui/QFontDatabase>
#include <QtGui/QGuiApplication>
#include <QtGui/QIcon>
#include <QtGui/QImage>
#include <QtGui/QPixmap>
#include <QtGui/QPixmapCache>
#include <QtQml/QQmlApplicationEngine>
#include <QtQml/QQmlEngine>
#include <QtQuick/QQuickWindow>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <malloc.h>
#endif

namespace {
QIcon &appIcon()
{
    static QIcon icon;
    return icon;
}

QPointer<QQmlEngine> &qmlEngineRef()
{
    static QPointer<QQmlEngine> engine;
    return engine;
}
} // namespace

void register_qml_engine(QQmlEngine *engine)
{
    if (engine)
        qmlEngineRef() = engine;
}

void register_qml_app_engine(QQmlApplicationEngine &engine)
{
    qmlEngineRef() = &engine;
}

#include <QtCore/QDateTime>
#include <QtCore/QFile>
#include <QtCore/QStandardPaths>
#include <QtCore/QTextStream>

namespace {
void qtLogMessageHandler(QtMsgType type, const QMessageLogContext &ctx, const QString &msg)
{
    if (type == QtDebugMsg)
        return;
    const QString dirPath = QStandardPaths::writableLocation(QStandardPaths::GenericDataLocation)
        + QStringLiteral("/Nectarlink/logs");
    QDir().mkpath(dirPath);
    QFile file(dirPath + QStringLiteral("/qml.log"));
    if (file.open(QIODevice::WriteOnly | QIODevice::Append | QIODevice::Text)) {
        QTextStream ts(&file);
        ts << QDateTime::currentDateTimeUtc().toString(Qt::ISODate) << " [" << type << "] "
           << msg << " (" << (ctx.file ? ctx.file : "") << ":" << ctx.line << ")\n";
    }
}
} // namespace

void prepare_qt()
{
    qInstallMessageHandler(qtLogMessageHandler);
    prepare_qt_without_log();
}

void prepare_qt_without_log()
{
    // Windows get an alpha channel so the Mica backdrop can show through, and
    // D3D swap chains are presented with DirectComposition, without which
    // transparent pixels render white (see spikes/s1-qt-rust, lesson 7).
    QQuickWindow::setDefaultAlphaBuffer(true);
    qputenv("QT_QPA_DISABLE_REDIRECTION_SURFACE", "1");
    qputenv("QSG_RENDER_LOOP", "basic");
    // Qt's V4 JS engine reserves 2 x QV4_JS_MAX_STACK_SIZE of committed RW
    // memory (8 MB by default). 512 KB per stack (1 MB total) is plenty for
    // UI bindings and saves 7 MB of private commit in the tray.
    qputenv("QV4_JS_MAX_STACK_SIZE", "524288");
}

void add_app_icon_image(int32_t size, rust::Slice<const uint8_t> rgba)
{
    if (size <= 0 || rgba.size() != static_cast<size_t>(size) * size * 4)
        return;
    // Copy: the image must not reference Rust memory after this call.
    const QImage image =
        QImage(rgba.data(), size, size, size * 4, QImage::Format_RGBA8888).copy();
    appIcon().addPixmap(QPixmap::fromImage(image));
}

void apply_app_icon()
{
    QGuiApplication::setWindowIcon(appIcon());
}

void keep_running_without_windows()
{
    QGuiApplication::setQuitOnLastWindowClosed(false);
}

int32_t load_bundled_fonts()
{
    int32_t loaded = 0;
    const QDir dir(QStringLiteral(":/fonts"));
    for (const QString &name : dir.entryList({QStringLiteral("*.ttf")}, QDir::Files)) {
        if (QFontDatabase::addApplicationFont(dir.filePath(name)) >= 0)
            ++loaded;
    }
    return loaded;
}

void trim_memory_caches()
{
    clear_idle_video_frames();
    QPixmapCache::clear();
    if (QQmlEngine *engine = qmlEngineRef().data()) {
        engine->collectGarbage();
        engine->trimComponentCache();
        engine->collectGarbage();
    }
#ifdef _WIN32
    _heapmin();
    HANDLE heaps[64];
    const DWORD count = GetProcessHeaps(64, heaps);
    for (DWORD i = 0; i < count && i < 64; ++i) {
        HeapCompact(heaps[i], 0);
    }
    SetProcessWorkingSetSize(GetCurrentProcess(), static_cast<SIZE_T>(-1), static_cast<SIZE_T>(-1));
#endif
}

bool start_external_drag(rust::Str file_path, rust::Str text_fallback)
{
    const QString path = QString::fromUtf8(file_path.data(), static_cast<qsizetype>(file_path.size())).trimmed();
    const QString text = QString::fromUtf8(text_fallback.data(), static_cast<qsizetype>(text_fallback.size()));
    if (path.isEmpty() && text.isEmpty())
        return false;

    auto *mime = new QMimeData();
    QPixmap dragPixmap;
    if (!path.isEmpty() && QFileInfo::exists(path)) {
        mime->setUrls({QUrl::fromLocalFile(path)});
        QImage img(path);
        if (!img.isNull()) {
            mime->setImageData(img);
            dragPixmap = QPixmap::fromImage(img.scaled(96, 96, Qt::KeepAspectRatio, Qt::SmoothTransformation));
        }
    } else if (!text.isEmpty()) {
        mime->setText(text);
    } else {
        delete mime;
        return false;
    }

    QDrag drag(QGuiApplication::instance());
    drag.setMimeData(mime);
    if (!dragPixmap.isNull())
        drag.setPixmap(dragPixmap);
    return drag.exec(Qt::CopyAction) != Qt::IgnoreAction;
}

bool install_system_translator(rust::Str dir)
{
    const QString folder = QString::fromUtf8(dir.data(), static_cast<qsizetype>(dir.size()));
    const QLocale locale = QLocale::system();

    // Qt's own strings: file dialogs, context menus, the standard buttons.
    // windeployqt stages a merged `qt_<locale>.qm` in `translations/` beside the
    // executable, while a build from source finds `qtbase_<locale>.qm` in Qt's
    // own folder, so try both names in both places.
    const QStringList qtDirs{
        folder + QStringLiteral("/translations"),
        QLibraryInfo::path(QLibraryInfo::TranslationsPath),
    };
    bool qtInstalled = false;
    for (const QString &qtDir : qtDirs) {
        for (const char *name : {"qtbase", "qt"}) {
            auto *qt = new QTranslator(QCoreApplication::instance());
            if (qt->load(locale, QString::fromLatin1(name), QStringLiteral("_"), qtDir)) {
                QCoreApplication::installTranslator(qt);
                qtInstalled = true;
                break;
            }
            delete qt;
        }
        if (qtInstalled)
            break;
    }

    // NectarLink's own strings, staged beside the executable. A locale with no
    // translation (`nectarlink_es_ES.qm` missing) simply stays English.
    auto *app = new QTranslator(QCoreApplication::instance());
    if (!app->load(locale, QStringLiteral("nectarlink"), QStringLiteral("_"), folder)) {
        delete app;
        return false;
    }
    return QCoreApplication::installTranslator(app);
}

rust::String translate(rust::Str context, rust::Str source)
{
    const QString ctx = QString::fromUtf8(context.data(), static_cast<qsizetype>(context.size()));
    const QString src = QString::fromUtf8(source.data(), static_cast<qsizetype>(source.size()));
    const QByteArray ctxUtf8 = ctx.toUtf8();
    const QByteArray srcUtf8 = src.toUtf8();
    const QString translated = QCoreApplication::translate(ctxUtf8.constData(), srcUtf8.constData());
    const QByteArray out = translated.toUtf8();
    return rust::String(out.constData(), static_cast<size_t>(out.size()));
}

namespace {
// The strings the Rust side looks up through translate(). lupdate reads this
// list through QT_TRANSLATE_NOOP, which is why the pairs live here and not in
// Rust: it keeps them extractable, so regenerating the .ts does not throw them
// away as obsolete. Keep this in step with the call sites.
[[maybe_unused]] const char *const kRustStrings[] = {
    QT_TRANSLATE_NOOP("Tray", "Open NectarLink"),
    QT_TRANSLATE_NOOP("Tray", "Command palette"),
    QT_TRANSLATE_NOOP("Tray", "Toggle Shelf"),
    QT_TRANSLATE_NOOP("Tray", "Take photo with phone"),
    QT_TRANSLATE_NOOP("Tray", "Scan document with phone"),
    QT_TRANSLATE_NOOP("Tray", "Find my phone"),
    QT_TRANSLATE_NOOP("Tray", "Open copied link on phone"),
    QT_TRANSLATE_NOOP("Tray", "Quit NectarLink"),
    QT_TRANSLATE_NOOP("Timeline", "With"),
    QT_TRANSLATE_NOOP("Timeline", "Opened from"),
    QT_TRANSLATE_NOOP("Timeline", "Opened on"),
    QT_TRANSLATE_NOOP("Timeline", "Saved from"),
    QT_TRANSLATE_NOOP("Timeline", "Recorded on"),
    QT_TRANSLATE_NOOP("Timeline", "From"),
    QT_TRANSLATE_NOOP("Timeline", "Sent to"),
    QT_TRANSLATE_NOOP("Timeline", "Photo"),
    QT_TRANSLATE_NOOP("Timeline", "Voice recording"),
    QT_TRANSLATE_NOOP("Timeline", "File"),
    QT_TRANSLATE_NOOP("Timeline", "Copied image"),
    QT_TRANSLATE_NOOP("Timeline", "Clip (removed from clipboard history)"),
    QT_TRANSLATE_NOOP("Timeline", "Session"),
    QT_TRANSLATE_NOOP("Timeline", "Phone webcam"),
    QT_TRANSLATE_NOOP("Timeline", "Screen mirroring"),
    QT_TRANSLATE_NOOP("Timeline", "App window"),
    QT_TRANSLATE_NOOP("Webcam", "Starting camera on %1…"),
    QT_TRANSLATE_NOOP("Webcam", "Streaming %1 × %2 · %3 fps from %4"),
    QT_TRANSLATE_NOOP("App", "Downloading the update…"),
    QT_TRANSLATE_NOOP("App", "Downloading the update · %1 of %2"),
    QT_TRANSLATE_NOOP("App", "My phone's colors"),
    QT_TRANSLATE_NOOP("App", "My phone's colors (%1)"),
};
}



