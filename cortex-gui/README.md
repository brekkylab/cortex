# cortex-gui

A desktop window onto a Cortex workspace and the agent that works in it.

```
npm install
npm run tauri dev      # 개발용 창
npm run tauri build    # 배포 번들
```

The Rust side is its own Cargo package (`src-tauri`), outside the workspace at the repository root; it takes `cortex` and `cortex-agents/hyperclova` by path. No FUSE is needed: stores are made by running `mem init` / `index init` against a host temp file and writing the bytes into the tree.

## 실행

`실행` 탭이 첫 화면이다. 사용자(부서)와 모델을 고르고 요청을 쓰면 HyperCLOVA X 가 그 사용자의 권한으로 트리를 읽고 보고서를 쓴다. 왼쪽 트리는 그 사용자가 보는 그대로이고, `관리자` 로 바꾸면 모든 파일과 열람 부서가 보인다. 파일을 누르면 오른쪽에 그 사용자로 읽은 내용이, 권한이 없으면 거절 사유가 뜬다. 오른쪽의 보고서·감사 로그·권한 대조는 실행이 끝나면 채워진다.

환경변수: `CLOVASTUDIO_API_KEY`(필수), `CORTEX_HCX_WORKSPACE`(트리 루트, 기본은 `cortex-agents/hyperclova/examples/procurement`), `CORTEX_HCX_S3`(`bucket[/prefix]` — 이 버킷이 `CORTEX_HCX_S3_AT`, 기본 `재무팀` 폴더를 대신한다), `CORTEX_HCX_S3_REGION`, `CORTEX_HCX_S3_ENDPOINT`, 그리고 `--s3` 를 쓸 때의 `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`.

## 워크스페이스

같은 트리를 관리자로 본다. 폴더·Notion·Naver Cloud Storage·S3 를 연결하고, 파일을 가져오거나 편집하고, memory·docset 저장소를 만든다. 실행이 남긴 산출물도 여기서 열 수 있다.

## 에이전트

모델(CLOVA Studio 의 HCX, 또는 로컬 AI 의 오픈웨이트)과 시스템 메시지, 읽을 리소스와 경로를 등록해 두고, `실행` 으로 실행 화면을 그 모델로 연다.

## 테마

시스템 설정을 따르고, 제목 바의 버튼으로 라이트·다크를 고정할 수 있다. 서체는 Geist(OFL)다.
