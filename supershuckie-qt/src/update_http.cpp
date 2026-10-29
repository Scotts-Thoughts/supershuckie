#include "update_http.hpp"

#include <QBuffer>
#include <QIODevice>
#include <vector>

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <winhttp.h>

namespace SuperShuckie64 {

namespace {

struct HInternet {
    HINTERNET handle = nullptr;
    HInternet(HINTERNET handle) : handle(handle) {}
    ~HInternet() { if(this->handle != nullptr) WinHttpCloseHandle(this->handle); }
    HInternet(const HInternet &) = delete;
    HInternet &operator=(const HInternet &) = delete;
    operator HINTERNET() const noexcept { return this->handle; }
};

QString last_error(const char *what) {
    DWORD code = GetLastError();
    switch(code) {
        case ERROR_WINHTTP_NAME_NOT_RESOLVED:
        case ERROR_WINHTTP_CANNOT_CONNECT:
            return QString("%1: could not connect to the server (are you online?)").arg(what);
        case ERROR_WINHTTP_TIMEOUT:
            return QString("%1: the server did not answer in time").arg(what);
        case ERROR_WINHTTP_SECURE_FAILURE:
            return QString("%1: the secure connection could not be set up").arg(what);
        default:
            return QString("%1 failed (WinHTTP error %2)").arg(what).arg(code);
    }
}

}

QString http_get(const QString &url, const char *accept, QIODevice *out, HttpProgress *progress) {
    std::wstring wurl = url.toStdWString();
    URL_COMPONENTS parts = {};
    parts.dwStructSize = sizeof(parts);
    parts.dwHostNameLength = static_cast<DWORD>(-1);
    parts.dwUrlPathLength = static_cast<DWORD>(-1);
    parts.dwExtraInfoLength = static_cast<DWORD>(-1);
    if(!WinHttpCrackUrl(wurl.c_str(), 0, 0, &parts) || parts.nScheme != INTERNET_SCHEME_HTTPS) {
        return QString("Not an https URL: %1").arg(url);
    }
    std::wstring host(parts.lpszHostName, parts.dwHostNameLength);
    std::wstring path(parts.lpszUrlPath, parts.dwUrlPathLength);
    if(parts.lpszExtraInfo != nullptr) {
        path.append(parts.lpszExtraInfo, parts.dwExtraInfoLength);
    }

    std::wstring agent = L"SuperShuckie/" + QString(SUPERSHUCKIE_VERSION).toStdWString();
    HInternet session(WinHttpOpen(agent.c_str(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_NO_PROXY_NAME, WINHTTP_NO_PROXY_BYPASS, 0));
    if(session == nullptr) {
        return last_error("Starting the connection");
    }
    WinHttpSetTimeouts(session, 15000, 15000, 30000, 30000);
    DWORD protocols = WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3;
    if(!WinHttpSetOption(session, WINHTTP_OPTION_SECURE_PROTOCOLS, &protocols, sizeof(protocols))) {
        // Windows 10 before 1903 does not know TLS 1.3; its defaults include 1.2.
        protocols = WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2;
        WinHttpSetOption(session, WINHTTP_OPTION_SECURE_PROTOCOLS, &protocols, sizeof(protocols));
    }

    HInternet connection(WinHttpConnect(session, host.c_str(), parts.nPort, 0));
    if(connection == nullptr) {
        return last_error("Connecting");
    }
    HInternet request(WinHttpOpenRequest(connection, L"GET", path.c_str(), nullptr, WINHTTP_NO_REFERER, WINHTTP_DEFAULT_ACCEPT_TYPES, WINHTTP_FLAG_SECURE));
    if(request == nullptr) {
        return last_error("Opening the request");
    }
    std::wstring headers = L"Accept: " + QString(accept).toStdWString() + L"\r\nX-GitHub-Api-Version: 2022-11-28\r\n";
    if(!WinHttpSendRequest(request, headers.c_str(), static_cast<DWORD>(-1), WINHTTP_NO_REQUEST_DATA, 0, 0, 0)
       || !WinHttpReceiveResponse(request, nullptr)) {
        return last_error("Downloading");
    }

    DWORD status = 0;
    DWORD size = sizeof(status);
    WinHttpQueryHeaders(request, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_HEADER_NAME_BY_INDEX, &status, &size, WINHTTP_NO_HEADER_INDEX);
    if(status < 200 || status >= 300) {
        if(status == 403 || status == 429) {
            return QString("GitHub refused the request (HTTP %1); it allows a limited number of checks per hour, so try again later").arg(status);
        }
        return QString("The server answered HTTP %1").arg(status);
    }

    if(progress != nullptr) {
        wchar_t length[32] = {};
        DWORD length_size = sizeof(length);
        if(WinHttpQueryHeaders(request, WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_HEADER_NAME_BY_INDEX, length, &length_size, WINHTTP_NO_HEADER_INDEX)) {
            progress->total = QString::fromWCharArray(length).toLongLong();
        }
    }

    std::vector<char> buffer(256 * 1024);
    while(true) {
        if(progress != nullptr && progress->cancel) {
            return "Cancelled";
        }
        DWORD read = 0;
        if(!WinHttpReadData(request, buffer.data(), static_cast<DWORD>(buffer.size()), &read)) {
            return last_error("Downloading");
        }
        if(read == 0) {
            break;
        }
        if(out->write(buffer.data(), read) != static_cast<qint64>(read)) {
            return QString("Could not write the download: %1").arg(out->errorString());
        }
        if(progress != nullptr) {
            progress->received += read;
        }
    }
    return {};
}

QString http_get(const QString &url, const char *accept, QByteArray &out) {
    QBuffer buffer(&out);
    buffer.open(QIODevice::WriteOnly);
    return http_get(url, accept, &buffer, nullptr);
}

}
