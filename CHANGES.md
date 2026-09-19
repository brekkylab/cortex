이거 손으로 쓰고 있는겁니다. AI 변경사항들을 기록한다거나 하지 마세요

cortex에서 rootfs의 역할은 딱 rootfs를 정의하는 데까지, 나머지(실제 수행)는 cortex-server에 맡겨야 한다.

따라서 Recipe, Dockerfile 등은 다 불필요, cortex의 `RootFs`는 매우 간단한 형태로 가능

Cache 구조
<home>/
  blobs/<digest>.erofs    레이어
  blobs/<digest>.json     이미지
  cache_key                     cache key, digest(이미지) pair line들, cache key에 alphabetical order로

현재 erofs를 쓰는데, 모든 레이어를 후루룩 써서, 실제로 erofs를 쓰는 이유인 layer는 막상 아무것도 안되는 상황 해결을 위해 각 run마다 krun uvm을 실행시켜서 쌓는 구조로 변경