# cortex-gui

A desktop window onto one cortex workspace: what a session can see, and the agents registered
against it.

```
npm install
npm run tauri dev      # 개발용 창
npm run tauri build    # 배포 번들
```

## 준비물

빌드에 **FUSE-T** 가 필요합니다 (`brew install --cask fuse-t`). memory·docset 의 저장소를
만들 때 워크스페이스를 잠깐 호스트에 마운트하기 때문입니다 — 아래 *리소스는 워크스페이스
안에 있습니다* 를 보세요. FUSE-T 의 `fuse-t.pc` 는 pkg-config 기본 검색 경로에 이미 있어
`PKG_CONFIG_PATH` 를 손댈 필요는 없습니다.

그리고 상위 워크스페이스의 실행 파일 셋이 빌드돼 있어야 합니다:

```
cd .. && cargo build -p cortex-local-console -p cortex-exec-mem -p cortex-exec-index
```

셋을 담은 디렉터리는 작업 디렉터리에서 위로 올라가며 찾은 `target/debug` 또는
`target/release` 입니다. 다른 곳에 있으면 `CORTEX_BIN_DIR` 로 지정하세요.

## 지금 무엇이 있는가

**워크스페이스 탭.** 창을 열면 언제나 빈 `WorkFs` 하나로 시작합니다 — 루트에 `InMemFs` 가
마운트된 트리이고, 창을 닫으면 사라집니다. 여기에

* 파일을 창에 끌어다 놓거나 `파일 추가` 로 가져옵니다. 선택된 폴더가 놓이는 위치입니다.
* `폴더 연결` 로 이 컴퓨터의 디렉터리를 (`PassthroughFs`), `Notion 연결` 로 워크스페이스를
  (`NotionFs`), `S3 연결` 로 버킷을 (`S3Fs`) 트리 아래에 붙입니다. 뒤의 둘은 읽기 전용이고,
  연결하기 전에 요청을 한 번 보내 자격 증명을 확인합니다.
* 텍스트 파일은 오른쪽에서 바로 편집하고 저장합니다.
* `+ memory` / `+ docset` 은 리소스를 **워크스페이스 안에** 만듭니다 — `mem init` /
  `index init` 이 쓰는 저장소 파일 `/.cortex/{memory,docset}/<이름>.sqlite` 하나입니다.
  목록은 매번 그 디렉터리를 읽어 오고, 트리에서 그대로 보이고, 지우면 파일이 지워집니다.

**에이전트 탭.** 등록한 memory·docset 과 워크스페이스 경로를 고르고, 시스템 메시지와 모델을
적어 에이전트를 등록합니다.

## 리소스는 저장소 파일 그 자체입니다

memory 하나 = 파일 하나. `/.cortex/memory/<이름>.sqlite` 이고, `mem init` 이 씁니다
(docset 은 `/.cortex/docset/` 과 `index init`). 곁에 매니페스트도, 목록도 두지 않습니다.

* 이름은 파일 이름 그대로입니다. 슬러그로 바꾸지 않으므로 입력한 대로 왕복하고, 창이 아무도
  고르지 않은 이름을 보여줄 일이 없습니다.
* 종류는 어느 디렉터리에 있는지가 말해 줍니다. 진짜 권위는 파일 안의 `meta.kind` 지만 그건
  마운트 없이는 못 읽으므로, `init` 이 종류에 맞는 디렉터리에 넣습니다.
* **"등록"과 "생성"이 같은 일입니다.** 아무것도 없는 상태와 저장소가 있는 상태 사이에 중간이
  없으니, 반쯤 등록된 리소스도, 재시도 버튼도, 서로 어긋날 두 번째 파일도 없습니다.
  `storebase` 의 `create` 는 배타적 생성이고 스키마가 안 들어가면 파일을 지우므로, 실패한
  `init` 은 워크스페이스를 건드리지 않은 것과 같습니다.

대신 **설명(note) 을 둘 곳이 없습니다.** `mem` 저장소의 `meta` 테이블에 자리는 있지만 거기
쓰려면 SQLite 를 열어야 하고, 그건 마운트와 그런 명령이 있는 프로그램을 뜻합니다. 그래서
리소스는 이름 하나입니다.

## 마운트가 필요한 이유

SQLite 는 *경로* 가 아니라 *파일* 을 엽니다 — 형제 저널을 만들고 락을 잡을 수 있는, 어떤
커널이 답해 주는 파일. 그래서 저장소를 만들 때 `store.rs` 는 작업 디렉터리 아래
`.cortex-mnt` 에 워크스페이스를 FUSE-T 로 마운트하고, `cortex-local-console` 세션에서
`mem init` 한 번을 돌리고, 마운트를 내립니다. 명령 하나의 길이만큼만 살아 있습니다.

이게 임시인 지점 둘. 세션 내내 마운트를 들고 있는 편이 (에이전트가 명령을 돌리게 되면 어차피
필요하므로) 결국 갈 방향이지만, 실패를 넘겨 살아남은 마운트는 누군가 손으로 `umount` 해야
하는 디렉터리이므로 지금은 범위를 좁혀 뒀습니다. 그리고 실행 파일을 PATH 가 아니라 `target/`
에서 찾는 것도 임시입니다 — 번들에는 Tauri sidecar 로 실어야 하고, 바꿀 곳은 `store.rs` 의
`bin_dir` 하나입니다.

## 지금 무엇이 없는가

두 가지가 자리만 잡혀 있고, 조용히 되는 척하지 않습니다 — 버튼은 비활성이거나 왜 안 되는지
말합니다.

* **세션 열기 / 저장.** 워크스페이스는 메모리에만 있습니다. 나중에 이 자리를 바꾸는 것은
  `Workspace::empty()` 하나이고, 그 위의 모든 코드는 트리를 경로로만 다루므로 아래에 무엇이
  있는지 묻지 않습니다. 지금은 창을 닫으면 만든 저장소까지 함께 사라집니다.
* **에이전트 실행.** ailoy 가 붙는 자리입니다. 지금 만드는 기록(시스템 메시지, 모델, 준 리소스와
  경로)이 곧 런타임의 입력이므로, 먼저 옳게 만들어 둘 값어치가 있는 부분이 그것입니다.

## 구조

```
src/                  React + TypeScript. api.ts 가 Rust 로 가는 유일한 통로.
src-tauri/src/
  state.rs            창이 가진 것: WorkFs 하나와 목록 셋(마운트·리소스·에이전트)
  fsops.rs            트리를 읽고 쓰는 명령, 그리고 호스트 파일 가져오기
  mounts.rs           커넥터: 무엇을 어디에 붙일 수 있는가
  agents.rs           리소스(트리 안의 저장소 파일)와 에이전트 등록
  store.rs            마운트 → mem/index init → 언마운트
  shared.rs           워크스페이스를 넘겨주지 않고 마운트 가능하게 만드는 핸들
```

`src-tauri` 는 상위 워크스페이스의 멤버가 아니라 그 자체로 워크스페이스입니다 — 이유는
`src-tauri/Cargo.toml` 에 적혀 있습니다. `cargo` 명령은 `src-tauri` 안에서 실행합니다.

마운트를 실제로 태우는 테스트는 `#[ignore]` 입니다 (FUSE-T 와 위의 실행 파일이 필요하므로):

```
cd src-tauri && cargo test -- --ignored --nocapture --test-threads=1
```

`--test-threads=1` 은 마운트 지점이 하나이고 마운트 지점은 비어 있어야 하기 때문입니다 —
창에서는 `Workspace::store_init` 이 직렬화하는 그 충돌입니다.
