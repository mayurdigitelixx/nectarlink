#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
stage="$(mktemp -d -t nectarlink-video-test-XXXXXX)"
trap 'rm -rf "${stage}"' EXIT
test_source="${root}/tests/linux_video_dispatch_test.cpp"

g++ -std=c++17 -Wall -Wextra -Werror -DTEST_BUILTIN -c "${test_source}" -o "${stage}/builtin.o"
g++ -std=c++17 -Wall -Wextra -Werror "${root}/cpp/linux_video_dispatch.cpp" \
    "${test_source}" "${stage}/builtin.o" -ldl -o "${stage}/test"
"${stage}/test" none
g++ -std=c++17 -Wall -Wextra -Werror -fPIC -shared -DTEST_PLUGIN \
    "${test_source}" -o "${stage}/libnectarlink-video-ffmpeg7.so"
g++ -std=c++17 -Wall -Wextra -Werror -fPIC -shared -DTEST_PLUGIN -DTEST_AVAILABLE -DTEST_LABEL='"ffmpeg8"' \
    "${test_source}" -o "${stage}/libnectarlink-video-ffmpeg8.so"
"${stage}/test" ffmpeg8
g++ -std=c++17 -Wall -Wextra -Werror -fPIC -shared -DTEST_PLUGIN -DTEST_AVAILABLE -DTEST_LABEL='"ffmpeg9"' \
    "${test_source}" -o "${stage}/libnectarlink-video-ffmpeg9.so"
"${stage}/test" ffmpeg9
g++ -std=c++17 -Wall -Wextra -Werror -DTEST_BUILTIN -DTEST_AVAILABLE -DTEST_LABEL='"builtin"' \
    -c "${test_source}" -o "${stage}/available-builtin.o"
g++ -std=c++17 -Wall -Wextra -Werror "${root}/cpp/linux_video_dispatch.cpp" \
    "${test_source}" "${stage}/available-builtin.o" -ldl -o "${stage}/test-with-builtin"
"${stage}/test-with-builtin" ffmpeg9
g++ -std=c++17 -Wall -Wextra -Werror "${root}/tests/linux_video_software_test.cpp" \
    $(pkg-config --cflags libavcodec libavutil) -ldl -o "${stage}/software-test"
"${stage}/software-test"
