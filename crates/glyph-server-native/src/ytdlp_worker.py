"""Private, newline-JSON audio resolver. No command flags come from callers."""
import contextlib
import json
import re
import shutil
import sys
from urllib.parse import urlsplit

sys.path.insert(0, shutil.which(sys.argv[1]) or sys.argv[1])
import yt_dlp


class Quiet:
    def debug(self, *_): pass
    def warning(self, *_): pass
    def error(self, *_): pass


def extractor(fast):
    args = ["--ignore-config", "--no-warnings", "--no-playlist", "-f",
            "bestaudio[ext=m4a]/bestaudio[acodec=vorbis]"]
    if fast:
        args += ["--extractor-args", "youtube:skip=hls,dash;player_skip=initial_data"]
    options = yt_dlp.parse_options(args).ydl_opts
    options.update(quiet=True, logger=Quiet(), socket_timeout=5, extractor_retries=1)
    return yt_dlp.YoutubeDL(options)


def resolve(ydl, video):
    with contextlib.redirect_stdout(sys.stderr):
        info = ydl.extract_info("https://www.youtube.com/watch?v=" + video, download=False)
    value = info.get("url", "")
    url = urlsplit(value)
    if len(value) > 60000 or url.scheme != "https" or not (url.hostname or "").endswith(".googlevideo.com"):
        raise ValueError("No direct audio URL")
    return value


with extractor(True) as fast, extractor(False) as normal:
    print(json.dumps({"ready": True}), flush=True)
    while True:
        line = sys.stdin.readline(1024)
        if not line:
            break
        video = ""
        try:
            video = json.loads(line)["id"]
            if not isinstance(video, str) or not re.fullmatch(r"[A-Za-z0-9_-]{11}", video):
                raise ValueError("Invalid video")
            try:
                value = resolve(fast, video)
            except Exception:
                value = resolve(normal, video)
            result = {"id": video, "url": value}
        except Exception:
            result = {"id": video, "error": "unavailable"}
        print(json.dumps(result), flush=True)
