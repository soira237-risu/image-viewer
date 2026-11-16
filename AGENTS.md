# AGENTS.md

이 저장소에서 작업하는 코딩 에이전트(Claude Code, Codex 등)를 위한 규칙. 사람에게도 그대로 유효함.

프로젝트: Rust로 만든 작고 빠른 Windows 이미지 뷰어. 구조와 개발 단계는 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), 사용법은 [README.md](README.md).

## 빌드 환경

- Rust stable (MSVC), Visual Studio 2022 Build Tools, NASM (`rav1d` 어셈블리)
- NASM이 PATH에 없으면 bash에서: `export PATH="$HOME/.cargo/bin:$(cygpath "$LOCALAPPDATA")/bin/NASM:$PATH"`
- 메모리가 빠듯하면 `-j 3`처럼 병렬 작업 수를 줄일 것 (커밋 메모리 부족 시 rustc가 0xC0000409로 죽음)

## 표준 명령 — 항상 워크스페이스 전체 범위로

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
```

- `-p <크레이트>`로 일부만 빌드하면 기능(feature) 조합이 달라져 같은 의존성이 한 벌 더 생김. 빠른 확인용으로는 써도 되지만, 기본은 `--workspace`
- 릴리스 빌드(`--release`)는 속도·메모리 측정이나 실행 파일 확인이 필요할 때만. 결과물이 5 GB 이상
- 커밋 전에는 위 세 명령이 모두 통과해야 함

## 디스크 사용 규칙 (중요)

Cargo는 옛 빌드 결과물을 지우지 않는다. 코드·기능 조합·빌드 대상이 바뀔 때마다 새 결과물이 옆에 쌓여 `target/`은 계속 커진다. 이 저장소에서 `target/debug`가 51 GB까지 커져 C: 드라이브를 가득 채운 적이 있다. 드라이브는 하나뿐이므로 다음을 지킨다.

1. **긴 작업의 시작과 끝에 용량 확인**

   ```bash
   pwsh -File scripts/cache-check.ps1          # 보고만 (target 20 GB 초과 또는 여유 30 GB 미만이면 종료 코드 1)
   pwsh -File scripts/cache-check.ps1 -Clean   # 한도를 넘었으면 cargo clean
   ```

   한도를 넘었으면 `-Clean`으로 정리하고 진행한다. 정리 후 첫 빌드는 몇 분 더 걸릴 뿐 결과는 같다.

2. **빌드 프로필의 디버그 정보를 늘리지 않는다.** `Cargo.toml`에서 의존성은 `debug = false`, 우리 크레이트는 `debug = "line-tables-only"`(패닉·백트레이스의 줄 번호는 남음). 디버거로 변수를 봐야 하면 로컬에서만 잠시 바꾸고 커밋하지 않는다.

3. **빌드 캐시를 늘리는 설정을 만들지 않는다.** `CARGO_TARGET_DIR` 변경, 다른 위치의 `target/`, 추가 git worktree는 캐시를 통째로 한 벌 더 만든다. 꼭 필요하면 작업이 끝난 뒤 그 캐시를 지운다.

4. **예제·벤치마크는 필요할 때만** `--example <이름>`으로 빌드한다 (`crates/iv-win/examples/`, `iv bench`).

5. **큰 테스트 파일은 저장소 밖이나 `testdata/local/`(git 제외)에.** 생성한 이미지·로그는 임시 폴더에 두고 끝나면 지운다. 커밋하는 테스트 데이터는 작고 합성된 것만.

6. **증상으로 디스크 부족을 의심할 것:** `no space on device`, `os error 112`, rustc가 메시지 없이 종료 코드 101/0xC0000409로 실패 → 먼저 `pwsh -File scripts/cache-check.ps1` 실행.

7. **지워도 되는 것은 `target/`뿐.** `~/.cargo`(레지스트리 캐시는 Cargo가 스스로 정리), 사용자 파일, 다른 프로젝트는 건드리지 않는다.

## 실제 창 테스트 규칙

사용자가 다른 프로그램(게임 등)을 쓰는 중일 수 있다.

- 뷰어는 항상 `--no-activate`로 띄운다 (포커스를 가져가지 않음)
- 입력은 `PostMessage`로 **뷰어 자신의 창에만** 보내고, 화면은 `PrintWindow`로 **뷰어 창만** 캡처한다. `SendKeys`·전역 키 입력·화면 전체 캡처 금지
- 우클릭 메뉴(창을 앞으로 가져옴), 파일 대화상자, 저장 확인 대화상자는 자동 시험하지 않는다. 편집한 상태로 끝낼 때는 프로세스를 강제 종료해 대화상자가 뜨지 않게 한다
- 시작·표시 시간은 `--trace`(stderr)로 잰다

## 코드 규칙

- 문서·README는 한국어, 코드·주석·커밋 메시지는 영어
- 화면에 보이는 문구는 `crates/iv-win/src/i18n.rs`의 `Strings`에 한국어·영어 둘 다 추가 (빠지면 컴파일 오류)
- 디코더·인코더는 `iv-codec`의 레지스트리에, 편집 연산은 `iv-ops`에, 화면 그리기는 `iv-win/src/raster.rs`에. 플랫폼 독립 부분은 창 없이 테스트한다
