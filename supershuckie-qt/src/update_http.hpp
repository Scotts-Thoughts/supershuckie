#ifndef __SUPERSHUCKIE_UPDATE_HTTP_HPP__
#define __SUPERSHUCKIE_UPDATE_HTTP_HPP__

#include <QByteArray>
#include <QString>
#include <atomic>
#include <cstdint>

class QIODevice;

namespace SuperShuckie64 {

/** Progress of one download, shared with the thread doing it. */
struct HttpProgress {
    std::atomic<std::int64_t> received = 0;
    std::atomic<std::int64_t> total = -1;  // -1 until the server says (Content-Length)
    std::atomic<bool> cancel = false;
};

/**
 * A blocking HTTPS GET through WinHTTP (system proxy settings, redirects followed). The body goes
 * to `out`. Returns an empty string on a 2xx response, otherwise what went wrong. Call it off the
 * GUI thread; `progress` may be null.
 */
QString http_get(const QString &url, const char *accept, QIODevice *out, HttpProgress *progress = nullptr);

/** http_get into memory. */
QString http_get(const QString &url, const char *accept, QByteArray &out);

}

#endif
