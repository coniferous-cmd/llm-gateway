"""Test the gateway using the official OpenAI Python SDK."""

import argparse
import os
import sys

from openai import APIError, APIStatusError, DefaultHttpxClient, OpenAI


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default="http://127.0.0.1:8080/v1",
                        help="OpenAI SDK base URL, including /v1")
    parser.add_argument("--token", default=os.environ.get("GATEWAY_TOKEN", "sk-virt-dev"))
    parser.add_argument("--model", default="mimo-v2.6-flash")
    parser.add_argument("--prompt", default="Reply with only: OK")
    parser.add_argument("--timeout", type=float, default=60)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")

    print("OpenAI SDK: base_url={}, model={}, auth=Bearer".format(
        args.base_url, args.model), flush=True)
    try:
        with OpenAI(
            api_key=args.token,
            base_url=args.base_url,
            timeout=args.timeout,
            max_retries=0,
            http_client=DefaultHttpxClient(trust_env=False),
        ) as client:
            # Read raw HTTP output without requiring SDK response-model validation.
            # This does not request SSE; stream=False keeps a single JSON response.
            with client.chat.completions.with_streaming_response.create(
                model=args.model,
                messages=[{"role": "user", "content": args.prompt}],
                max_completion_tokens=64,
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
