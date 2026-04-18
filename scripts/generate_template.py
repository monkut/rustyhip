#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "pyyaml>=6.0",
# ]
# ///
"""Generate an AWS SAM template for the RustyHip Lambda + HttpApi stack.

Runtime-only values (the S3 bucket name and DB object key) stay as
CloudFormation `Parameters` so the same template can be reused across
environments. Structural knobs (memory, timeout, architecture, runtime)
are baked in at generate time because changing them requires re-cutting
the Lambda zip anyway.

Usage:
    uv run scripts/generate_template.py                     # write to stdout
    uv run scripts/generate_template.py --output template.yaml
    uv run scripts/generate_template.py --architecture x86_64 --memory-mb 1024

Then deploy with SAM:
    sam deploy --template-file template.yaml \\
        --stack-name rustyhip \\
        --capabilities CAPABILITY_IAM \\
        --resolve-s3 \\
        --parameter-overrides BucketName=my-bucket DbKey=rustyhip.db
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Any

import yaml

# Classmethod billing tag — identifies which project the resource belongs to.
DEFAULT_PROJECT_ID = "2daec5cf-78b5-4cdc-96be-06b7cefb6eb1"
CM_BILLING_GROUP_TAG = "CmBillingGroup"


def build_tags(project_id: str) -> dict[str, str]:
    return {CM_BILLING_GROUP_TAG: f"ProjectId={project_id}"}


def build_template(
    *,
    function_logical_id: str,
    function_name: str,
    api_logical_id: str,
    api_name: str,
    architecture: str,
    runtime: str,
    memory_mb: int,
    timeout_s: int,
    code_uri: str,
    handler: str,
    log_level: str,
    project_id: str,
) -> dict[str, Any]:
    # build_tags() per call so each resource emits its own inline block (no YAML anchors).
    return {
        "AWSTemplateFormatVersion": "2010-09-09",
        "Transform": "AWS::Serverless-2016-10-31",
        "Description": "RustyHip — SQLite-over-S3 Lambda fronted by API Gateway HTTP API",
        "Parameters": {
            "BucketName": {
                "Type": "String",
                "Description": "S3 bucket holding the turbolite-managed database pages.",
                "AllowedPattern": r"^[a-z0-9.\-]{3,63}$",
            },
            "DbName": {
                "Type": "String",
                "Description": "Turbolite prefix (logical database name) within the bucket.",
                "MinLength": 1,
            },
        },
        "Resources": {
            api_logical_id: {
                "Type": "AWS::Serverless::HttpApi",
                "Properties": {
                    "Name": api_name,
                    "StageName": "$default",
                    "Tags": build_tags(project_id),
                },
            },
            function_logical_id: {
                "Type": "AWS::Serverless::Function",
                "Properties": {
                    "FunctionName": function_name,
                    "CodeUri": code_uri,
                    "Handler": handler,
                    "Runtime": runtime,
                    "Architectures": [architecture],
                    "MemorySize": memory_mb,
                    "Timeout": timeout_s,
                    # rustyhip serializes writes through /tmp/rustyhip.db + S3 PutObject.
                    # Running more than one container at a time would race the upload (last-writer-wins)
                    # and let stale /tmp state serve reads that miss another container's writes.
                    "ReservedConcurrentExecutions": 1,
                    "Environment": {
                        "Variables": {
                            "BUCKET": {"Ref": "BucketName"},
                            "DB_NAME": {"Ref": "DbName"},
                            "LOG_LEVEL": log_level,
                        },
                    },
                    # Turbolite reads + writes many page objects under the DbName prefix.
                    "Policies": [
                        {"S3CrudPolicy": {"BucketName": {"Ref": "BucketName"}}},
                    ],
                    "Events": {
                        "Root": {
                            "Type": "HttpApi",
                            "Properties": {"ApiId": {"Ref": api_logical_id}, "Path": "/", "Method": "ANY"},
                        },
                        "Proxy": {
                            "Type": "HttpApi",
                            "Properties": {"ApiId": {"Ref": api_logical_id}, "Path": "/{proxy+}", "Method": "ANY"},
                        },
                    },
                    "Tags": build_tags(project_id),
                },
            },
        },
        "Outputs": {
            "ApiEndpoint": {
                "Description": "Invoke URL for the default HttpApi stage.",
                "Value": {
                    "Fn::Sub": "https://${" + api_logical_id + "}.execute-api.${AWS::Region}.amazonaws.com/",
                },
            },
            "ApiName": {
                "Description": "HttpApi physical name.",
                "Value": api_name,
            },
            "FunctionName": {
                "Description": "Lambda function name.",
                "Value": {"Ref": function_logical_id},
            },
            "FunctionArn": {
                "Description": "Lambda function ARN.",
                "Value": {"Fn::GetAtt": [function_logical_id, "Arn"]},
            },
        },
    }


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--function-logical-id", default="RustyHipFunction", help="CloudFormation logical ID for the Lambda resource.")
    p.add_argument("--function-name", default="rhp-rustyhip", help="Deployed Lambda function name (physical, uses rhp- project prefix).")
    p.add_argument("--api-logical-id", default="RustyHipHttpApi", help="CloudFormation logical ID for the HttpApi resource.")
    p.add_argument("--api-name", default="rhp-rustyhip-api", help="Deployed HttpApi physical name (uses rhp- project prefix).")
    p.add_argument("--architecture", choices=["arm64", "x86_64"], default="arm64")
    p.add_argument("--runtime", default="provided.al2023", help="Lambda runtime for the custom Rust bootstrap.")
    p.add_argument("--memory-mb", type=int, default=512)
    p.add_argument("--timeout-s", type=int, default=30)
    p.add_argument("--code-uri", default="./target/lambda/rustyhip", help="Path sam will upload for CodeUri.")
    p.add_argument("--handler", default="bootstrap", help="Lambda handler identifier (ignored for provided.* but required by schema).")
    p.add_argument("--log-level", default="info")
    p.add_argument(
        "--project-id",
        default=DEFAULT_PROJECT_ID,
        help=f"Classmethod project UUID emitted as {CM_BILLING_GROUP_TAG}=ProjectId=<uuid> on every resource.",
    )
    p.add_argument("-o", "--output", type=Path, default=None, help="Write template to this path (default: stdout).")
    return p.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    template = build_template(
        function_logical_id=args.function_logical_id,
        function_name=args.function_name,
        api_logical_id=args.api_logical_id,
        api_name=args.api_name,
        architecture=args.architecture,
        runtime=args.runtime,
        memory_mb=args.memory_mb,
        timeout_s=args.timeout_s,
        code_uri=args.code_uri,
        handler=args.handler,
        log_level=args.log_level,
        project_id=args.project_id,
    )
    rendered = yaml.safe_dump(template, sort_keys=False, default_flow_style=False, width=120, allow_unicode=True)
    if args.output is None:
        sys.stdout.write(rendered)
    else:
        args.output.write_text(rendered, encoding="utf-8")
        print(f"wrote {args.output}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
