#!/usr/bin/env python3
"""查询小爱音箱官方 OTA 包的下载信息（只查询，不下载，不需要登录）。

用法：
    python -I scripts/ota-query.py <SN> <当前版本> [channel]
    python -I scripts/ota-query.py - 1.94.13          # SN 用 - 表示只按型号+版本查

型号默认 LX06，可用环境变量 LX06_MODEL 覆盖。

返回 JSON 里：
    data.currentInfo  当前版本对应的完整包（link / hash / size）
    data.upgradeInfo  可以升级到的版本
下载后必须核对 hash（MD5）。文件名形如 mico_all_911151b95_1.94.13.bin，不要改名：
解包脚本用文件名里的 MD5 片段做校验。

2026-11-16 实测：LX06 + 1.94.13 返回
    mico_all_911151b95_1.94.13.bin（43478328 字节，MD5 b202de2bbe149ce884aa500911151b95）
"""
import base64
import hashlib
import json
import os
import sys
import time
import urllib.request

SALT = "8007236f-a2d6-4847-ac83-c49395ad6d65"


def query(model, sn, version, channel):
    t = int(time.time() * 1000)
    info = (f"channel={channel}&filterID={sn}&locale=zh_CN&model={model}"
            f"&time={t}&version={version}&{SALT}")
    s = hashlib.md5(base64.b64encode(info.encode())).hexdigest()
    url = (f"http://api.miwifi.com/rs/grayupgrade/v2/{model}?model={model}"
           f"&version={version}&channel={channel}&filterID={sn}"
           f"&locale=zh_CN&time={t}&s={s}")
    with urllib.request.urlopen(url, timeout=20) as r:
        return json.loads(r.read())


def main(argv):
    if len(argv) < 3:
        print(__doc__)
        return 2
    model = os.environ.get("LX06_MODEL", "LX06")
    sn = "" if argv[1] == "-" else argv[1]
    version = argv[2]
    channel = argv[3] if len(argv) > 3 else "release"
    result = query(model, sn, version, channel)
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0 if result.get("code") == "0" and result.get("data") else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
