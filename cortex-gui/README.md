# cortex-gui

A desktop window onto one cortex workspace: what a session can see, and the agents registered
against it.

```
npm install
npm run tauri dev      # 개발용 창
npm run tauri build    # 배포 번들
```

## 지금 무엇이 있는가

**워크스페이스 탭.** 창을 열면 언제나 빈 `WorkFs` 하나로 시작합니다 — 루트에 `InMemFs` 가
마운트된 트리이고, 창을 닫으면 사라집니다. 여기에

* 파일을 창에 끌어다 놓거나 `파일 추가` 로 가져옵니다. 선택된 폴더가 놓이는 위치입니다.
* `폴더 연결` 로 이 컴퓨터의 디렉터리를 (`PassthroughFs`), `Notion 연결` 로 워크스페이스를
  (`NotionFs`), `S3 연결` 로 버킷을 (`S3Fs`) 트리 아래에 붙입니다. 뒤의 둘은 읽기 전용이고,
  연결하기 전에 요청을 한 번 보내 자격 증명을 확인합니다.
* 텍스트 파일은 오른쪽에서 바로 편집하고 저장합니다.
* `+ memory` / `+ docset` 은 리소스를 **워크스페이스 안에** 만듭니다 —
  `/.cortex/{memory,docset}/<이름>.json` 파일 하나이고, 목록은 매번 그 디렉터리를 읽어 옵니다.
  트리에서 그대로 보이고, 지우면 파일이 지워집니다.

**에이전트 탭.** 등록한 memory·docset 과 워크스페이스 경로를 고르고, 시스템 메시지와 모델을
적어 에이전트를 등록합니다.

## 지금 무엇이 없는가

세 가지가 자리만 잡혀 있습니다. 어느 것도 조용히 되는 척하지 않습니다 — 버튼은 비활성이거나
`준비 중` 이라고 말합니다.

* **세션 열기 / 저장.** 워크스페이스는 메모리에만 있습니다. 나중에 이 자리를 바꾸는 것은
  `Workspace::empty()` 하나이고, 그 위의 모든 코드는 트리를 경로로만 다루므로 아래에 무엇이
  있는지 묻지 않습니다.
* **memory · docset 의 실제 저장소.** 리소스 자체는 워크스페이스 안에 있지만, 옆에 놓일
  `<이름>.sqlite` — `cortex-execs/mem` 과 `cortex-execs/index` 가 쓰는 파일 — 은 아직
  없습니다. `준비 중` 배지는 하드코딩이 아니라 그 파일이 트리에 있는지 읽어서 붙으므로,
  저장소가 어떤 경로로든 생기면 이 창은 코드 수정 없이 그렇게 표시합니다.
* **에이전트 실행.** ailoy 가 붙는 자리입니다. 지금 만드는 기록(시스템 메시지, 모델, 준 리소스와
  경로)이 곧 런타임의 입력이므로, 먼저 옳게 만들어 둘 값어치가 있는 부분이 그것입니다.

## 구조

```
src/                  React + TypeScript. api.ts 가 Rust 로 가는 유일한 통로.
src-tauri/src/
  state.rs            창이 가진 것: WorkFs 하나와 목록 셋(마운트·리소스·에이전트)
  fsops.rs            트리를 읽고 쓰는 명령, 그리고 호스트 파일 가져오기
  mounts.rs           커넥터: 무엇을 어디에 붙일 수 있는가
  agents.rs           리소스(트리 안의 파일)와 에이전트 등록
```

`src-tauri` 는 상위 워크스페이스의 멤버가 아니라 그 자체로 워크스페이스입니다 — 이유는
`src-tauri/Cargo.toml` 에 적혀 있습니다. `cargo` 명령은 `src-tauri` 안에서 실행합니다.
