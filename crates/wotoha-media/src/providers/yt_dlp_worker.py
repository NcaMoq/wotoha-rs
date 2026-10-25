"""Private JSON-lines bridge for the opt-in resident yt-dlp API experiment."""

import json
import http.client
import os
import resource
import socket
import ssl
import sys
import time
import urllib.parse


ACTIVE_PROFILE = None
PROFILE_FIELDS = (
    "http_requests",
    "http_open_ms",
    "http_body_read_ms",
    "dns_calls",
    "dns_ms",
    "tcp_connect_calls",
    "tcp_connect_ms",
    "tls_handshakes",
    "tls_ms",
    "js_solver_calls",
    "js_solver_ms",
    "js_runtime_calls",
    "js_runtime_ms",
    "cpu_user_ms",
    "cpu_system_ms",
    "child_cpu_user_ms",
    "child_cpu_system_ms",
    "rss_highwater_kb",
    "child_rss_highwater_kb",
    "youtube_requests",
    "media_cdn_requests",
    "other_requests",
)
MAX_REQUEST_DETAILS = 8


def new_profile():
    profile = dict.fromkeys(PROFILE_FIELDS, 0)
    profile["http_request_details"] = []
    return profile


def add_time(field, started):
    if ACTIVE_PROFILE is not None:
        ACTIVE_PROFILE[field] += (time.perf_counter() - started) * 1000.0


def wrap_timed_call(owner, name, field, counter=None):
    original = getattr(owner, name)

    def wrapped(*args, **kwargs):
        profile = ACTIVE_PROFILE
        started = time.perf_counter() if profile is not None else None
        if profile is not None and counter:
            profile[counter] += 1
        try:
            return original(*args, **kwargs)
        finally:
            if profile is not None:
                profile[field] += (time.perf_counter() - started) * 1000.0

    setattr(owner, name, wrapped)


def install_internal_profiling():
    wrap_timed_call(socket, "getaddrinfo", "dns_ms", "dns_calls")
    wrap_timed_call(socket.socket, "connect", "tcp_connect_ms", "tcp_connect_calls")
    wrap_timed_call(ssl.SSLContext, "wrap_socket", "tls_ms", "tls_handshakes")

    try:
        from yt_dlp.extractor.youtube.jsc._builtin.ejs import EJSBaseJCP

        wrap_timed_call(EJSBaseJCP, "_run_js_runtime", "js_runtime_ms", "js_runtime_calls")

        original_bulk_solve = EJSBaseJCP._real_bulk_solve

        def timed_bulk_solve(self, requests, /):
            profile = ACTIVE_PROFILE
            started = time.perf_counter() if profile is not None else None
            if profile is not None:
                profile["js_solver_calls"] += 1
            try:
                yield from original_bulk_solve(self, requests)
            finally:
                if profile is not None:
                    profile["js_solver_ms"] += (time.perf_counter() - started) * 1000.0

        EJSBaseJCP._real_bulk_solve = timed_bulk_solve
    except (ImportError, AttributeError):
        pass


def host_category(url):
    try:
        host = urllib.parse.urlsplit(str(url)).hostname or ""
    except ValueError:
        return "other_requests"
    host = host.lower().rstrip(".")
    if host == "googlevideo.com" or host.endswith(".googlevideo.com"):
        return "media_cdn_requests"
    if (
        host == "youtube.com"
        or host.endswith(".youtube.com")
        or host == "youtubei.googleapis.com"
        or host.endswith(".youtubei.googleapis.com")
    ):
        return "youtube_requests"
    return "other_requests"


def request_kind(url):
    """Return a low-cardinality route label; never retain or emit URL/path data."""
    try:
        parsed = urllib.parse.urlsplit(str(url))
        host = (parsed.hostname or "").lower().rstrip(".")
        path = parsed.path
    except ValueError:
        return "other"
    if host == "googlevideo.com" or host.endswith(".googlevideo.com"):
        return "media_cdn"
    if "/youtubei/v1/player" in path:
        return "youtube_player_api"
    if "/youtubei/v1/config" in path:
        return "youtube_client_config"
    if "/youtubei/v1/next" in path:
        return "youtube_initial_data"
    if path.startswith("/s/player/"):
        return "youtube_player_script"
    if path == "/watch" or path.startswith("/watch/"):
        return "youtube_webpage"
    if (
        host == "youtube.com"
        or host.endswith(".youtube.com")
        or host == "youtubei.googleapis.com"
    ):
        return "youtube_other"
    return "other"


def instrument_downloader(ydl):
    original_urlopen = ydl.urlopen

    def timed_urlopen(request):
        profile = ACTIVE_PROFILE
        detail = None
        if profile is not None:
            profile["http_requests"] += 1
            profile[host_category(getattr(request, "url", request))] += 1
            if len(profile["http_request_details"]) < MAX_REQUEST_DETAILS:
                detail = {
                    "kind": request_kind(getattr(request, "url", request)),
                    "status_code": 0,
                    "open_us": 0,
                    "body_read_us": 0,
                }
                profile["http_request_details"].append(detail)
        started = time.perf_counter() if profile is not None else None
        try:
            response = original_urlopen(request)
        finally:
            if profile is not None:
                elapsed_us = int((time.perf_counter() - started) * 1_000_000.0)
                profile["http_open_ms"] += elapsed_us / 1000.0
                if detail is not None:
                    detail["open_us"] = elapsed_us

        if profile is not None and response is not None:
            if detail is not None:
                status = getattr(response, "status", None)
                if status is None:
                    status = getattr(response, "code", None)
                if isinstance(status, int) and 0 <= status <= 65535:
                    detail["status_code"] = status
            original_read = response.read

            def timed_read(*args, **kwargs):
                started_read = time.perf_counter()
                try:
                    return original_read(*args, **kwargs)
                finally:
                    add_time("http_body_read_ms", started_read)
                    if detail is not None:
                        detail["body_read_us"] += int(
                            (time.perf_counter() - started_read) * 1_000_000.0
                        )

            try:
                response.read = timed_read
            except (AttributeError, TypeError):
                pass
        return response

    ydl.urlopen = timed_urlopen


def selected_request_handler(ydl):
    try:
        from yt_dlp.networking import Request

        handlers = ydl._request_director._get_handlers(Request("https://www.youtube.com/"))
        return handlers[0].RH_NAME if handlers else "none"
    except Exception:
        return "unknown"


def usage_snapshot(who):
    try:
        usage = resource.getrusage(who)
        return usage.ru_utime, usage.ru_stime, usage.ru_maxrss
    except (AttributeError, OSError, ValueError):
        return 0.0, 0.0, 0


def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":"), allow_nan=False) + "\n")
    sys.stdout.flush()


def classify(error):
    text = str(error).lower()
    if "confirm your age" in text or "age-restricted" in text or "age restricted" in text:
        return "Sign in to confirm your age"
    if any(token in text for token in ("sign in", "authentication required", "login required", "members-only", "members only")):
        return "Sign in required"
    if any(token in text for token in ("private video", "video is private", "video unavailable", "not available in your country", "geo-restricted", "geoblocked")):
        return "Video unavailable in this region"
    if "429" in text or "too many requests" in text or "rate limit" in text:
        return "HTTP Error 429: Too Many Requests"
    if any(token in text for token in ("requested format is not available", "no video formats found", "no formats found")):
        return "Requested format is not available"
    return "yt-dlp Python API extraction failed"


def main():
    started = time.perf_counter()
    from yt_dlp import YoutubeDL
    from yt_dlp.version import __version__

    import_ms = (time.perf_counter() - started) * 1000.0

    started = time.perf_counter()
    init = json.loads(sys.stdin.readline())
    if init.get("op") != "init" or init.get("id") != 0:
        raise RuntimeError("invalid worker initialization")

    params = {
        "ignoreconfig": True,
        "noplaylist": True,
        "skip_download": True,
        "quiet": True,
        "no_warnings": True,
        "socket_timeout": 10,
        "retries": 1,
        "extractor_retries": 1,
        "format": "bestaudio[protocol^=http]/bestaudio/best",
        "extractor_args": {"youtube": {"skip": ["hls", "dash"]}},
    }
    if init.get("cookies_file"):
        params["cookiefile"] = init["cookies_file"]
    if init.get("deno_path"):
        params["js_runtimes"] = {"deno": {"path": init["deno_path"]}}

    ydl = YoutubeDL(params)
    profile_enabled = os.environ.get("WOTOHA_YTDLP_INTERNAL_PROFILE") == "1"
    handler_name = selected_request_handler(ydl)
    if profile_enabled:
        install_internal_profiling()
        instrument_downloader(ydl)
    send({"id": 0, "ok": True, "version": __version__, "startup_ms": (time.perf_counter() - started) * 1000.0, "import_ms": import_ms, "request_handler": handler_name})

    try:
        for line in sys.stdin:
            try:
                request = json.loads(line)
                request_id = request.get("id")
                if request.get("op") != "extract" or not isinstance(request_id, int):
                    send({"id": request_id, "ok": False, "error": "invalid worker request", "api_extract_ms": 0.0})
                    continue

                ydl.params["extractor_args"] = (
                    {"youtube": {"skip": ["hls", "dash"]}} if request.get("fast", True) else {}
                )
                profile = new_profile() if profile_enabled else None
                global ACTIVE_PROFILE
                ACTIVE_PROFILE = profile
                started = time.perf_counter()
                user_before = usage_snapshot(resource.RUSAGE_SELF)
                child_before = usage_snapshot(resource.RUSAGE_CHILDREN)
                try:
                    info = ydl.extract_info(request["url"], download=False)
                    api_extract_ms = (time.perf_counter() - started) * 1000.0
                    if not isinstance(info, dict):
                        raise RuntimeError("yt-dlp returned no single-track information")
                    sanitized = ydl.sanitize_info(info)
                    selected = {
                        key: sanitized.get(key)
                        for key in (
                            "id", "title", "uploader", "channel", "webpage_url", "thumbnail",
                            "duration", "is_live", "url", "protocol", "http_headers", "filesize",
                            "filesize_approx",
                        )
                    }
                    result = {"id": request_id, "ok": True, "info": selected, "api_extract_ms": api_extract_ms}
                except Exception as error:
                    api_extract_ms = (time.perf_counter() - started) * 1000.0
                    result = {"id": request_id, "ok": False, "error": classify(error), "api_extract_ms": api_extract_ms}
                finally:
                    if profile is not None:
                        user_after = usage_snapshot(resource.RUSAGE_SELF)
                        child_after = usage_snapshot(resource.RUSAGE_CHILDREN)
                        profile["cpu_user_ms"] = (user_after[0] - user_before[0]) * 1000.0
                        profile["cpu_system_ms"] = (user_after[1] - user_before[1]) * 1000.0
                        profile["child_cpu_user_ms"] = (child_after[0] - child_before[0]) * 1000.0
                        profile["child_cpu_system_ms"] = (child_after[1] - child_before[1]) * 1000.0
                        profile["rss_highwater_kb"] = user_after[2]
                        profile["child_rss_highwater_kb"] = child_after[2]
                        result["profile"] = {
                            **{
                                key: value
                                for key, value in profile.items()
                                if not key.endswith("_ms")
                            },
                            **{
                                key[:-3] + "_us": int(value * 1000.0)
                                for key, value in profile.items()
                                if key.endswith("_ms")
                            },
                            "http_request_details": profile["http_request_details"][:MAX_REQUEST_DETAILS],
                        }
                    ACTIVE_PROFILE = None
                send(result)
            except Exception:
                # Never echo request content or upstream exception text to either output stream.
                send({"id": None, "ok": False, "error": "invalid worker message", "api_extract_ms": 0.0})
    finally:
        ydl.close()


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        # Startup failures intentionally omit tracebacks and configuration values.
        sys.exit(70)
