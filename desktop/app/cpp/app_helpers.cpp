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

// Every QML engine registered, for retranslating them all when the
// language changes (gone ones drop out on their own).
QList<QPointer<QQmlEngine>> &allEngines()
{
    static QList<QPointer<QQmlEngine>> engines;
    return engines;
}

void remember(QQmlEngine *engine)
{
    auto &engines = allEngines();
    engines.removeIf([](const QPointer<QQmlEngine> &e) { return e.isNull(); });
    if (!engines.contains(engine))
        engines.append(engine);
}
} // namespace

void register_qml_engine(QQmlEngine *engine)
{
    if (engine) {
        qmlEngineRef() = engine;
        remember(engine);
    }
}

void register_qml_app_engine(QQmlApplicationEngine &engine)
{
    qmlEngineRef() = &engine;
    remember(&engine);
}

#include <QtCore/QDateTime>
#include <QtCore/QFile>
#include <QtCore/QStandardPaths>
#include <QtCore/QTextStream>

#include <atomic>
#include <cstdio>

namespace {
std::atomic<int> &qmlProblems()
{
    static std::atomic<int> count { 0 };
    return count;
}

// A warning or error from QML: a component that didn't load, a binding or
// a script that failed.
bool isQmlProblem(QtMsgType type, const QMessageLogContext &ctx, const QString &msg)
{
    if (type != QtWarningMsg && type != QtCriticalMsg && type != QtFatalMsg)
        return false;
    return (ctx.file && QByteArray(ctx.file).startsWith("qrc:/")) || msg.contains(QStringLiteral("qrc:/"));
}

// `--ui-check`: messages go to stderr, not the log file, and QML problems
// are counted.
void uiCheckMessageHandler(QtMsgType type, const QMessageLogContext &ctx, const QString &msg)
{
    if (type == QtDebugMsg)
        return;
    if (isQmlProblem(type, ctx, msg))
        qmlProblems().fetch_add(1);
    std::fprintf(stderr, "[%d] %s (%s:%d)\n", int(type), qUtf8Printable(msg), ctx.file ? ctx.file : "", ctx.line);
    std::fflush(stderr);
}

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

void prepare_qt_for_ui_check()
{
    qInstallMessageHandler(uiCheckMessageHandler);
    prepare_qt_without_log();
}

int32_t qml_problem_count()
{
    return qmlProblems().load();
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

namespace {
// The translators installed now, replaced when the language changes.
QPointer<QTranslator> &qtTranslator()
{
    static QPointer<QTranslator> t;
    return t;
}
QPointer<QTranslator> &appTranslator()
{
    static QPointer<QTranslator> t;
    return t;
}

void drop(QPointer<QTranslator> &translator)
{
    if (translator) {
        QCoreApplication::removeTranslator(translator);
        delete translator.data();
    }
    translator = nullptr;
}
} // namespace

bool install_translator(rust::Str dir, rust::Str language)
{
    const QString folder = QString::fromUtf8(dir.data(), static_cast<qsizetype>(dir.size()));
    const QString chosen = QString::fromUtf8(language.data(), static_cast<qsizetype>(language.size()));
    const QLocale locale = chosen.isEmpty() ? QLocale::system() : QLocale(chosen);
    QLocale::setDefault(locale);
    drop(qtTranslator());
    drop(appTranslator());

    // Qt's own strings: file dialogs, context menus, the standard buttons.
    // windeployqt stages a merged `qt_<locale>.qm` in `translations/` beside the
    // executable, while a build from source finds `qtbase_<locale>.qm` in Qt's
    // own folder, so try both names in both places.
    const QStringList qtDirs{
        folder + QStringLiteral("/translations"),
        QLibraryInfo::path(QLibraryInfo::TranslationsPath),
    };
    for (const QString &qtDir : qtDirs) {
        for (const char *name : {"qtbase", "qt"}) {
            auto *qt = new QTranslator(QCoreApplication::instance());
            if (qt->load(locale, QString::fromLatin1(name), QStringLiteral("_"), qtDir)) {
                QCoreApplication::installTranslator(qt);
                qtTranslator() = qt;
                break;
            }
            delete qt;
        }
        if (qtTranslator())
            break;
    }

    // NectarLink's own strings, staged beside the executable. A locale with no
    // translation (`nectarlink_es_ES.qm` missing), or English, stays English.
    bool installed = false;
    auto *app = new QTranslator(QCoreApplication::instance());
    if (app->load(locale, QStringLiteral("nectarlink"), QStringLiteral("_"), folder)) {
        installed = QCoreApplication::installTranslator(app);
        appTranslator() = app;
    } else {
        delete app;
    }

    // Windows already open show the new language at once.
    for (const QPointer<QQmlEngine> &engine : allEngines()) {
        if (engine)
            engine->retranslate();
    }
    return installed;
}

rust::String translate(rust::Str context, rust::Str text)
{
    const std::string c(context.data(), context.size());
    const std::string t(text.data(), text.size());
    return rust::String(QCoreApplication::translate(c.c_str(), t.c_str()).toStdString());
}


