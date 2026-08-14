# Architecture — `fs`

`cortex::fs`는 작고 합성 가능한 가상 파일시스템입니다.

한 문장으로 요약하면 **"경로로 주소를 매기는 아무 저장소든 진짜 파일시스템으로 노출한다"**
입니다. 저장소 쪽은 [`Mountable`](mountable.rs) trait 하나로 표현하고, 노출 쪽은
인터페이스마다 얇은 바인딩을 하나씩 얹습니다.

이 크레이트의 다른 절반은 `console`입니다 — 명령을 실행하는 채널이고,
[`console/ARCHITECTURE.md`](../console/ARCHITECTURE.md)가 그쪽을 다룹니다. 둘은 서로를
모릅니다: 여기 있는 것 중 무엇도 console을 언급하지 않고, 그 반대도 마찬가지입니다.

## 계층

```mermaid
flowchart TB
    subgraph surface["바인딩 — 인터페이스별"]
        fuse["mounts/fuse.rs<br/><i>fuser Filesystem</i><br/>커널 FUSE"]
        fuset["mounts/fuse_t.rs<br/><i>libfuse-t + C shim</i><br/>nfs · fskit · smb"]
        dav["webdav <i>(예정)</i><br/>HTTP"]
    end

    posix["<b>Posix&lt;T&gt;</b> — posix.rs<br/>inode 번호 · 커널 참조 카운트 · 열린 파일 테이블<br/>open 분해 · 접근 모드 · readdir 커서 · errno 표"]

    mountable["<b>Mountable</b> — mountable.rs<br/>stat · list · read_at<br/>create · mkdir · unlink · rmdir · rename · write_at · truncate · flush"]

    subgraph stores["구현체"]
        ws["<b>WorkFs</b> — workfs.rs<br/>최장 접두사 마운트 테이블"]
        mem["InMemFs"]
        pass["PassthroughFs"]
        s3["S3Fs <i>(읽기 전용)</i>"]
        notion["NotionFs <i>(읽기 전용)</i>"]
    end

    fuse --> posix
    fuset --> posix
    posix --> mountable
    dav -.->|"Posix를 건너뜀"| mountable
    mountable --- ws
    mountable --- mem
    mountable --- pass
    mountable --- s3
    mountable --- notion
    ws -->|"합성"| mem
    ws -->|"합성"| pass
```

점선이 이 구조에서 가장 중요한 부분입니다. **`Posix`는 공통 기반이 아니라 "커널은 파일을
번호로, 그리고 디스크립터로 지칭한다"는 사실 때문에 필요한 번역 계층**입니다. 모든 동작이
경로를 들고 오는 인터페이스(WebDAV의 HTTP 메서드, 라이브러리 호출)는 그 번역이 필요 없으므로
`Mountable`에 곧바로 닿고, `Posix`를 지나지 않습니다.

**게스트로 나가는 길은 없습니다.** microVM이 이 트리를 필요로 하면 다른 무엇과도 같은 방법으로
받습니다 — 호스트에 마운트한 뒤 디렉터리로 넘기는 것. `Posix`를 지나는 VM 모양의 두 번째 경로를
이 경로와 나란히 유지할 이유가 없습니다.

## 파일

| 파일 | 담고 있는 것 |
| --- | --- |
| [`mountable.rs`](mountable.rs) | `Mountable` — 저장소가 구현하는 유일한 계약. `Stat`·`DirentKind`·`Dirent`가 그것이 답하는 어휘로 함께 있습니다 |
| [`posix.rs`](posix.rs) | `Posix<T>` — inode·파일 핸들·open 분해·errno 표. `OpenOptions`·`SetAttr`은 *호출자의* open과 커널 `setattr`의 어휘이고 여기서 멈춥니다 — 저장소는 보지 않습니다 |
| [`mounts/`](mounts/) | 바인딩. 각각 가드 하나를 내보내고, `mod.rs`가 동기 콜백에서 async 저장소로 건너가는 유일한 지점(`block_on`)을 갖습니다 |
| [`stores/`](stores/) | 구체 저장소들 |
| [`workfs.rs`](workfs.rs) | `WorkFs` — 여러 저장소를 한 트리로 접합하며 그 자체로 저장소 |

## 설계 판단은 코드 옆에 있습니다

이 문서는 지도이고, *왜*는 각 항목의 doc comment가 답합니다. 특히 읽을 값이 있는 것들:

- **`Mountable`의 `# No opens, only paths`** — 저장소에 디스크립터가 없는 이유, 그리고 경로로
  per-open 상태를 캐싱하려는 저장소가 왜 반드시 틀리는지.
- **`Mountable`의 `# Durability`와 `# What a store answers with`** — 쓰기가 반환 시점에
  durable인 이유, `flush`가 약속이 아닌 이유, `ReadOnlyFilesystem`과 `Unsupported`를 userspace가
  다르게 취급한다는 것.
- **`Posix::unlink_child`** — unlink-while-open이 아직 없다는 것과, 그것을 되찾는 일이 왜 저장소가
  아니라 이 층의 몫인지.
- **`Posix::host_errno`** — raw errno를 넘겨도 되는 이유(같은 호스트의 번호 체계). 게스트를 읽는
  바인딩이 생기면 반대로 kind로만 분류해야 합니다 — `ENOTEMPTY`가 macOS 66, Linux 39입니다.
- **`mounts/fuse_t.rs`의 `FuseTBackend`** — 같은 vtable 위에서 전송만 바뀐다는 것.
- **`examples/compare_fuse_t_backends.rs`** — 그 전송들이 실제로 무엇을 요청하는지 측정한 표.
