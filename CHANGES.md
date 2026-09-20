이거 손으로 쓰고 있는겁니다. AI 변경사항들을 기록한다거나 하지 마세요

cortex에서 rootfs의 역할은 딱 rootfs를 정의하는 데까지, 나머지(실제 수행)는 cortex-server에 맡겨야 한다.

따라서 Recipe, Dockerfile 등은 다 불필요, cortex의 `RootFs`는 매우 간단한 형태로 가능

Cache 구조 (microsandbox-image의 GlobalCache와 동일하게)
