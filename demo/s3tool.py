#!/usr/bin/env python3
"""Small boto3 helper for md-sync S3 live tests.

Reads credentials from ~/workspace/rustfs-s3-config.json (the same file
s3_upload.py uses). Subcommands: get <key> | put <key> <file> |
exists <key> | list <prefix> | clean <prefix> | env
"""
import json
import os
import sys

import boto3
from botocore.config import Config

BUCKET = "museai"
CFG = os.path.expanduser("~/workspace/rustfs-s3-config.json")


def client():
    cfg = json.load(open(CFG))
    return boto3.client(
        "s3",
        endpoint_url=cfg["url"],
        aws_access_key_id=cfg["accessKey"],
        aws_secret_access_key=cfg["secretKey"],
        region_name="us-east-1",
        config=Config(s3={"addressing_style": "path"}, signature_version="s3v4"),
    )


def main():
    cmd = sys.argv[1]
    s3 = client()
    if cmd == "get":
        sys.stdout.buffer.write(
            s3.get_object(Bucket=BUCKET, Key=sys.argv[2])["Body"].read()
        )
    elif cmd == "put":
        with open(sys.argv[3], "rb") as f:
            s3.put_object(Bucket=BUCKET, Key=sys.argv[2], Body=f.read())
    elif cmd == "exists":
        try:
            s3.head_object(Bucket=BUCKET, Key=sys.argv[2])
            print("yes")
        except Exception:
            print("no")
    elif cmd == "list":
        p = s3.list_objects_v2(Bucket=BUCKET, Prefix=sys.argv[2])
        for o in p.get("Contents", []):
            print(o["Key"])
    elif cmd == "clean":
        prefix = sys.argv[2]
        while True:
            p = s3.list_objects_v2(Bucket=BUCKET, Prefix=prefix)
            objs = p.get("Contents", [])
            if not objs:
                break
            s3.delete_objects(
                Bucket=BUCKET,
                Delete={"Objects": [{"Key": o["Key"]} for o in objs]},
            )
            if not p.get("IsTruncated"):
                break
        print(f"cleaned {prefix}")
    elif cmd == "env":
        # Export lines for the shell. The caller evals them; values are
        # never echoed by this script or by the test script.
        cfg = json.load(open(CFG))
        print(f"export AWS_ACCESS_KEY_ID='{cfg['accessKey']}'")
        print(f"export AWS_SECRET_ACCESS_KEY='{cfg['secretKey']}'")
        print("export AWS_REGION=us-east-1")
        print(f"export AWS_ENDPOINT_URL={cfg['url']}")
    else:
        raise SystemExit(f"unknown subcommand: {cmd}")


if __name__ == "__main__":
    main()
