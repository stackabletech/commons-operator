#!/usr/bin/env sh

get_revision () {
    resource="$1"

    kubectl exec test-0 \
        --namespace "$NAMESPACE" \
        --container test -- \
        cat "/config/$resource/revision"
}

assert_revision () {
    expected_value="$1"
    resource="$2"

    actual_value="$(get_revision "$resource")"
    if test "$expected_value" = "$actual_value"
    then
        echo "[PASS] $resource contains expected value"
    else
        echo "[FAIL] $resource does not contain expected value: " \
             "expected: $expected_value != actual: $actual_value"
        exit 1
    fi
}
