"""Test the gateway using the official Anthropic Python SDK."""

import argparse
import os
import sys

from anthropic import APIError, APIStatusError, Anthropic, DefaultHttpxClient, omit


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default="http://127.0.0.1:8080/anthropic")
    parser.add_argument("--token", default=os.environ.get("GATEWAY_TOKEN", "sk-virt-dev"))
    parser.add_argument("--model", default="mimo-v2.6-flash")
    parser.add_argument("--prompt", default="Reply with only: OK")
    parser.add_argument(
        "--auth-mode", choices=("bearer", "api-key"), default="bearer",
        help="bearer matches current gateway; api-key tests native x-api-key auth",
    )
    parser.add_argument("--timeout", type=float, default=60)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")

    print("Anthropic SDK: base_url={}, model={}, auth={}".format(
        args.base_url, args.model, args.auth_mode), flush=True)
    try:
        with Anthropic(
            # Empty strings suppress fallback to real credentials from environment.
            api_key=args.token if args.auth_mode == "api-key" else "",
            auth_token=args.token if args.auth_mode == "bearer" else "",
            # Do not send the inactive authentication header, even with an empty value.
            default_headers={"x-api-key": omit} if args.auth_mode == "bearer" else {"Authorization": omit},
            base_url=args.base_url,
            timeout=args.timeout,
            max_retries=0,
            http_client=DefaultHttpxClient(trust_env=False),
        ) as client:
            # The SDK supplies anthropic-version and the selected authentication header.
            # Read the raw HTTP body; stream=False means this is not an SSE request.
            with client.messages.with_streaming_response.create(
                model=args.model,
                messages=[{"role": "user", "content": args.prompt}],
                max_tokens=64,
                stream=False,
            ) as response:
                print("HTTP status: {}".format(response.status_code))
                for name, value in response.headers.multi_items():
                    print("{}: {}".format(name, value))
                print()
                print(response.read().decode("utf-8", errors="replace"))
                return 0 if 200 <= response.status_code < 300 else 1
    except APIStatusError as error:
        print("HTTP status: {}".format(error.status_code))
        for name, value in error.response.headers.multi_items():
            print("{}: {}".format(name, value))
        print()
        print(error.response.text)
        return 1
    except (APIError, OSError) as error:
        print("Request failed: {}".format(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")
    sys.exit(main())
