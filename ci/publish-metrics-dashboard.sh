#!/usr/bin/env bash
set -e

cd "$(dirname "$0")/.."

if [[ -z $BUILDKITE ]]; then
  echo BUILDKITE not defined
  exit 1
fi

if [[ -z $CHANNEL ]]; then
  CHANNEL=$(buildkite-agent meta-data get "channel" --default "")
fi

if [[ -z $CHANNEL ]]; then
  (
    cat <<EOF
steps:
  - block: "Select Dashboard"
    fields:
      - select: "Channel"
        key: "channel"
        options:
          - label: "stable"
            value: "stable"
          - label: "edge"
            value: "edge"
          - label: "beta"
            value: "beta"
  - command: "ci/$(basename "$0")"
EOF
  ) | buildkite-agent pipeline upload
  exit 0
fi


ci/channel-info.sh
eval "$(ci/channel-info.sh)"

case $CHANNEL in
edge)
  CHANNEL_BRANCH=$EDGE_CHANNEL
  ;;
beta)
  CHANNEL_BRANCH=$BETA_CHANNEL
  ;;
stable)
  CHANNEL_BRANCH=$STABLE_CHANNEL
  ;;
*)
  echo "Error: Invalid CHANNEL=$CHANNEL"
  exit 1
  ;;
esac

if [[ $BUILDKITE_BRANCH != "$CHANNEL_BRANCH" ]]; then
  (
    cat <<EOF
steps:
  - trigger: "$BUILDKITE_PIPELINE_SLUG"
    async: true
    build:
      message: "$BUILDKITE_MESSAGE"
      branch: "$CHANNEL_BRANCH"
      env:
        CHANNEL: "$CHANNEL"
EOF
  ) | buildkite-agent pipeline upload
  exit 0
fi

set -x
exec metrics/publish-metrics-dashboard.sh "$CHANNEL"
