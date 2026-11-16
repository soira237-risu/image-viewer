# image-viewer

Rust로 만드는 작고 빠른 Windows 이미지 뷰어. GPU 드라이버도 UI 툴킷도 쓰지 않는 순수 Win32 창: 사진 한 장 띄운 상태 메모리 약 43 MB, 실행 후 50 ms 안에 사진 표시.

- **속도 우선**: 저해상도 미리보기 먼저 표시, 나선형 미리 읽기, GC 없는 일정한 반응성
- **다양한 형식**: JPEG, PNG, WebP, GIF, TIFF, AVIF, HEIC, JPEG XL, RAW, SVG 등
- **간단한 편집**: 회전, 뒤집기, 자르기, 크기 변경, 기본 보정, 실행 취소/다시 실행
- **형식 변환**: 다른 이름으로 저장, 잘못된 확장자 바로잡기(Ctrl+E), 일괄 변환(명령줄 `iv convert`)

## 상태

M0~M6 완료:

- 빠른 보기: CPU 렌더러(보이는 부분만 SIMD 리샘플, 크게 줄일 때는 절반·4분의 1 크기 단계에서, GDI로 표시), 100%는 화면 픽셀에 맞춰 원본 그대로(번짐 없음), 미리보기 먼저, 방향 고려 미리 읽기(메모리 예산에 들어가는 이웃만), 키를 누르고 있을 때의 빠른 넘김
- 16비트·부동소수 이미지는 화면용 8비트로 바꿔 캐시(디코드 작업자에서, 메모리 절반 이하). 저장은 파일을 다시 읽어 원래 정밀도로
- 형식: JPEG, PNG/APNG, GIF, WebP, AVIF, HEIC, JPEG XL, TIFF, BMP, ICO, QOI, TGA, PNM, DDS, EXR, HDR, SVG, 카메라 RAW, ICC 색 관리 (HEIC와 RAW는 Windows 코덱으로 읽음: [선택 코덱](#선택-코덱-heicraw) 참고)
- 압축파일: ZIP/CBZ, 7z/CB7 (만화 두 쪽 보기, 7z는 페이지가 든 압축 블록만 풂). `\`로 구분한 항목 이름(일부 Windows 도구)도 열림. 압축파일 밖을 가리키는 이름(`..`, `C:`, `/`로 시작)의 항목은 목록에서 빠짐. 항목 하나는 1 GiB까지 읽음. 7z의 헤더(파일 목록)는 풀어서 17 MiB까지(헤더 압축 사전 16 MiB까지, py7zr 기본값 포함), 압축 사전은 1 GiB까지만 받음
- SVG: 문서 안에 `data:`로 넣은 이미지만 그림. 바깥 파일·네트워크 경로(`\\서버\...`)를 가리키는 이미지는 읽지 않고 빈 칸으로 둠
- 편집: 회전·뒤집기·자르기·보정·크기 변경, 실행 취소, 저장/다른 이름으로 저장 (JPEG 회전은 무손실)
- 변환: 9개 형식 인코더, 확장자 바로잡기(뷰어 Ctrl+E, `iv fix-ext`), 일괄 변환(`iv convert`, 명령줄 전용)

설계와 개발 단계는 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), 개발 규칙(빌드 캐시 용량 관리 포함)은 [AGENTS.md](AGENTS.md), 버전별 변경은 [CHANGELOG.md](CHANGELOG.md) 참고.

## 설치

GitHub 릴리스에서 `image-viewer-<버전>-x86_64-windows.zip`을 받아 원하는 폴더에 풀고 `image-viewer.exe`를 실행. 설치 과정·관리자 권한·Visual C++ 재배포 패키지가 필요 없음 (C 런타임이 실행 파일 안에 들어 있음).

- 요구 사항: Windows 10 1703 이상 또는 Windows 11, 64비트(x64)
- zip 내용: `image-viewer.exe`(뷰어), `iv.exe`(명령줄 도구: 일괄 변환·확장자 바로잡기·정보·속도 측정), `README.md`, `CHANGELOG.md`, `LICENSE`, `THIRD-PARTY-NOTICES.txt`·`RUST-STD-COPYRIGHT.html`(실행 파일에 들어 있는 오픈 소스 패키지의 라이선스)
- 받은 파일 확인: 같은 이름의 `.sha256` 파일에 든 값과 `Get-FileHash <zip 파일>`의 값이 같아야 함
- `…-symbols.zip`은 비정상 종료 기록(`crash.log`)을 분석할 때 쓰는 디버그 기호(.pdb). 뷰어를 쓰는 데는 필요 없음. 배포한 실행 파일에는 .pdb가 없어 `crash.log`의 뷰어 프레임은 이름 대신 `[image-viewer.exe+0x…]`(실행 파일 시작 주소로부터의 오프셋)로 남고, 보고서 머리의 `image:` 줄에 시작 주소·링크 시각·.pdb의 GUID가 있음. 같은 릴리스의 `image-viewer.exe`와 풀어 둔 기호 폴더로 `pwsh -File scripts/symbolize.ps1 -CrashLog <crash.log> -Exe <image-viewer.exe> -Symbols <기호 폴더>`를 실행하면 Windows의 `dbghelp.dll`로 각 프레임에 함수 이름과 파일:줄을 붙여 보여 줌 (디버거 설치 불필요, 다른 빌드의 crash.log면 경고)
- 기본 뷰어로 쓰려면 [기본 뷰어로 쓰기](#기본-뷰어로-쓰기) 참고. 지울 때는 등록을 해제(`image-viewer --unregister`)한 뒤 폴더를 지우고, 설정(`%APPDATA%\image-viewer`)과 기록(`%LOCALAPPDATA%\image-viewer`)도 필요 없으면 지움

### 선택 코덱 (HEIC·RAW)

HEIC/HEIF와 카메라 RAW는 뷰어 안에 디코더가 없고 Windows 이미지 코덱(WIC)으로 읽음. Microsoft Store에서 아래 확장을 설치해야 열림. 없으면 열 때 제목 표시줄에 어떤 확장이 필요한지 표시됨.

| 형식 | 필요한 확장 (Microsoft Store) |
|---|---|
| HEIC/HEIF (아이폰 사진 등) | **HEIF 이미지 확장** + **HEVC 비디오 확장** (HEVC는 PC에 따라 유료) |
| 카메라 RAW (CR2·CR3·NEF·ARW·RAF·ORF·RW2·DNG 등) | **Raw 이미지 확장** |

AVIF·WebP·JPEG XL을 비롯한 나머지 형식은 모두 뷰어 안의 디코더로 읽으므로 따로 설치할 것이 없음. RAW는 아직 실제 카메라 파일로 검증하지 못함.

## 소스에서 빌드

필요: Rust 1.98 이상 (MSVC 툴체인), Visual Studio 2022 Build Tools (C++ 워크로드), NASM (AV1 디코더 `rav1d`의 어셈블리 최적화용).
C 런타임 정적 링크 등 모든 빌드에 공통인 설정은 `.cargo/config.toml`에 있음 (`RUSTFLAGS` 환경 변수를 쓰면 이 설정이 무시됨).

```bash
winget install NASM.NASM   # PATH에 없으면 환경 변수 NASM에 nasm.exe 경로 지정
```

```bash
cargo test
cargo build --release
./target/release/iv formats            # 형식별 등록 디코더
./target/release/iv probe <파일|폴더>   # 형식·크기·방향, 확장자 불일치 표시
./target/release/iv bench <파일|폴더>   # probe / preview / decode 시간 측정
./target/release/iv convert --to webp --opt quality=82 --max 1920 --out out <파일|폴더>   # 일괄 변환
./target/release/iv convert --to jpg --no-metadata --out out <파일|폴더>   # 위치(EXIF)·XMP·ICC·주석을 지운 사본 (JPEG→JPEG은 다시 압축하지 않음)
./target/release/iv fix-ext --dry-run <파일|폴더>   # 내용과 다른 확장자 찾기 (빼면 실제로 바꿈)
./target/release/image-viewer <파일|폴더>    # 뷰어 (--trace: 시작 단계별 시간 출력, --no-activate: 포커스를 가져가지 않고 열기)
./target/release/image-viewer --version     # 버전 (`iv --version`도 같음)
./target/release/image-viewer --register    # "연결 프로그램"·기본 앱 목록에 등록 (이 사용자만, --unregister로 해제)
```

### 릴리스 만들기

1. `Cargo.toml`의 `version`을 올리고 `CHANGELOG.md`에 `## [버전]` 절을 씀 (그 절이 릴리스 설명이 됨)
2. `v<버전>` 태그를 push → [`.github/workflows/release.yml`](.github/workflows/release.yml)이 테스트, 패키지, GitHub 릴리스(zip·`.sha256`·symbols zip)까지 진행. 수동 실행(workflow_dispatch)은 태그를 골라 실행해도 릴리스를 만들지 않고(태그·버전 일치만 확인) 결과물만 작업 아티팩트로 남김. 게시는 태그 push 이벤트에서만

같은 과정을 로컬에서: `pwsh -File scripts/package.ps1 -Jobs 3` → `target/dist/`. [scripts/package.ps1](scripts/package.ps1)이 순서대로 확인하고 하나라도 어긋나면 멈춤:

- 저장소 최상위에 `LICENSE` 파일이 있는지 (**아직 라이선스가 정해지지 않아 지금은 여기서 실패함**. 소유자가 라이선스를 고르고 `LICENSE`와 `Cargo.toml`의 `license`를 추가해야 릴리스 가능)
- 태그와 `Cargo.toml` 버전이 같은지, `CHANGELOG.md`에 그 버전 절이 있는지, `rust-version`이 잠긴 의존성들의 최소 Rust 버전 이상인지
- 깨끗한 릴리스 빌드(`target/package-build`, 성공·실패·Ctrl+C 어느 쪽으로 끝나도 지움. `-KeepBuild`면 남김): 빌드한 컴퓨터의 경로(사용자 폴더·Cargo 폴더·저장소 위치)를 `--remap-path-prefix`로 지움 (.pdb는 rustc가 원래 파일 이름만 넣음)
- 두 실행 파일이 C 런타임 DLL(`VCRUNTIME140.dll` 등)을 가져다 쓰지 않는지, 빌드 경로가 들어 있지 않은지, 버전이 맞는지
- 서드파티 고지: [scripts/notices.ps1](scripts/notices.ps1)이 `Cargo.lock`과 로컬 Cargo 레지스트리에서 실행 파일에 들어가는 패키지(빌드 스크립트·프로시저 매크로·테스트 전용 제외)의 라이선스와 전문을 모아 `THIRD-PARTY-NOTICES.txt`를 씀. 허용 목록 밖의 라이선스나 전문이 없는 패키지가 있으면 실패

### 기본 단축키

| 키 | 동작 |
|---|---|
| Ctrl+O | 파일 열기 |
| 파일·폴더 끌어다 놓기 | 열기 (폴더·압축파일은 첫 이미지부터) |
| →, Space, PageDown / ←, Backspace, PageUp | 다음 / 이전 (누르고 있으면 빠른 넘김) |
| 마우스 앞으로 / 뒤로 버튼 | 다음 / 이전 |
| Home / End | 처음 / 마지막 |
| R, Ctrl+R / L, Ctrl+L | 시계 / 반시계 방향 회전 |
| H / V | 좌우 / 상하 뒤집기 |
| Shift+드래그, C 또는 Enter | 영역 선택, 선택 영역으로 자르기 |
| Ctrl+Z / Ctrl+Y | 실행 취소 / 다시 실행 |
| Ctrl+S / Ctrl+Shift+S | 저장 / 다른 이름으로 저장 |
| Ctrl+E | 확장자 바로잡기 (내용은 WebP인데 이름이 `.jpg`이면 `.webp`로 이름만 바꿈, 같은 이름의 파일은 덮어쓰지 않음) |
| Delete | 휴지통으로 옮기기 (먼저 물음, 설정에서 끌 수 있음; 저장하지 않은 편집이 있으면 항상 물음). 화면에 보이는 이미지만 옮기고, 누르고 있어도 한 장만. 다음 이미지로 넘어감 (마지막이었으면 이전 이미지) |
| Ctrl+C | 이미지 복사 (보이는 그대로: 편집 적용, 투명도 유지) |
| Ctrl+Alt+C | 파일 복사 (탐색기에 붙여 넣으면 파일이 복사됨) |
| Ctrl+Shift+C | 경로 복사 (텍스트) |
| Ctrl+Shift+E | 폴더에서 보기 (탐색기에서 파일 선택) |
| F, Ctrl+0 | 창에 맞춤 |
| 1, Ctrl+1 | 100% |
| +, =, Ctrl++ / -, Ctrl+- | 확대 / 축소 |
| 휠 | 커서 기준 확대·축소 (설정에서 "휠로 사진 넘기기"를 켜면 이전·다음, 확대·축소는 Ctrl+휠) |
| 드래그 | 이동 |
| 더블클릭 | 맞춤 ↔ 100% |
| F11, Alt+Enter | 전체 화면 |
| D / Shift+D | 두 쪽 보기 / 읽는 방향 전환 |
| E | 편집 패널 (보정 슬라이더, 크기 변경, 저장 품질) |
| 우클릭 | 모든 명령 메뉴 (단축키 표시) |
| F2 | 설정 |
| F1 | 단축키 도움말 (지금 키맵 그대로의 키·마우스 목록) |
| Shift+F1 | 정보 (버전, 설정·기록 폴더) |
| Esc | 선택 해제 → 전체 화면 해제 → 종료 |
| Ctrl+W, Ctrl+Q | 종료 |

한글 입력 상태(IME 한글 모드)여도 R·F·D 같은 한 글자 단축키가 그대로 동작함 (뷰어 창에서는 입력기를 끔).

저장(Ctrl+S)은 파일 내용의 실제 형식으로 씀: 이름이 `.jpg`인 WebP는 WebP 그대로(투명도·무손실 유지). 제목 표시줄에 "확장자가 내용과 다름"이 보이면 Ctrl+E로 이름을 바로잡을 수 있음. 움직이는 이미지(GIF·WebP·APNG·JPEG XL, 첫 프레임만 저장되므로 원본 보호), 압축파일 안의 페이지, 쓸 수 없는 형식(HEIC·RAW 등)은 그 자리에 저장하지 않고 이유를 보여 준 뒤 다른 이름으로 저장을 엶. 너무 커서 첫 프레임만 보여 주는 애니메이션도 마찬가지. GIF와 움직이는 WebP·PNG는 다 읽기 전에는 몇 프레임인지 알 수 없으므로 저장하지 않고 다 읽은 뒤 다시 저장하라고 알려 줌 (저장 확인에서 "예"를 골라도 다음·이전·열기·종료는 진행하지 않음). 저장하는 동안에는 편집·실행 취소·두 번째 저장·이름 바꾸기를 받지 않고, 다음·이전·열기·종료는 저장이 끝난 뒤 이어서 진행함 (저장이 실패하면 진행하지 않고 편집은 남음).

설정은 F2 또는 `%APPDATA%\image-viewer\settings.toml` (`--settings <파일>`로 다른 파일 지정).
설정 파일이나 키맵 파일을 읽지 못하면 시작할 때 제목 표시줄에 이유(파일·줄 번호)가 첫 명령 전까지 표시됨. 읽지 못한 설정 파일은 설정 창에서 저장할 때 `settings.toml.bak`(있으면 `.bak2`, …)으로 보관한 뒤 새로 씀. 설정 파일은 임시 파일에 쓴 뒤 바꿔치기하므로 저장 도중 꺼져도 망가지지 않음.
창 위치·크기·최대화 상태는 닫을 때 설정 파일 옆의 `window.toml`에 따로 기록되어 다음 실행 때 그대로 열림 (`settings.toml`은 설정 창에서 저장할 때만 쓰므로 직접 고친 값·주석은 그대로 남음) (전체 화면은 기억하지 않음, 연결이 끊긴 모니터 위치는 무시). 처음 열 때는 주 모니터 배율에 맞춘 1280×800(논리 픽셀) 크기로 가운데에 열림.
오류 기록: 콘솔 없이 실행되므로 열기·저장·폴더 읽기 실패 같은 진단 내용은 `%LOCALAPPDATA%\image-viewer\image-viewer.log`에도 남음 (무언가 기록할 때만 생김, 256 KiB를 넘으면 다음 실행에서 비우고 새로 씀, 한 번 실행에 128 KiB까지). 뷰어가 비정상 종료하면 `crash.log`(같은 폴더, 마지막 한 건만 보관)에 위치·백트레이스(프레임 주소와 실행 파일 기준 오프셋, 실행 파일의 시작 주소·.pdb 식별자)를 쓰고 그 경로를 알리는 창을 띄움. 기록을 쓰지 못하면(폴더 없음·디스크 가득 참·권한 없음) 창은 파일을 남겼다고 하지 않고 그 이유와 패닉 메시지·위치를 보여 줌. 손상된 파일 때문에 디코더가 패닉해도 그 이미지만 "열 수 없습니다"로 실패하고 뷰어는 계속 동작함.

### 기본 뷰어로 쓰기

설정(F2)의 "Windows '연결 프로그램' 목록"에서 **등록**을 누르면(또는 `image-viewer --register`) 지금 실행 중인 image-viewer.exe가 지원하는 모든 이미지·압축파일 확장자의 "연결 프로그램" 목록과 Windows 설정 > 기본 앱에 나타남. 현재 사용자(`HKEY_CURRENT_USER`)에만 쓰므로 관리자 권한이 필요 없음. Windows 10/11은 프로그램이 스스로 기본 앱이 되는 것을 막으므로, 기본 뷰어로 쓰려면 **기본 앱 설정…** 버튼으로 연 화면에서 image-viewer를 고름. **등록 해제**(`--unregister`)는 등록이 쓴 값만 지우고(다른 프로그램의 항목과 사용자가 고른 기본 앱 설정은 그대로) 원래대로 돌림. 실행 파일을 옮겼으면 다시 등록.

화면 언어는 한국어가 기본이고 설정(F2)의 "언어"에서 English로 바꿀 수 있음 (바로 적용, `settings.toml`의 `language = "ko"` / `"en"`).

단축키는 `%APPDATA%\image-viewer\keymap.toml`로 바꿀 수 있음 (`--keymap <파일>`로 다른 파일 지정):

```toml
[bindings]
"Q" = "app.quit"
"F" = ""          # 기본 바인딩 해제
"Ctrl+WheelDown" = "nav.next"   # 마우스: WheelUp, WheelDown, Mouse4(뒤로), Mouse5(앞으로)
```

키맵에서 휠에 명령을 묶으면 그 조합의 휠은 확대 대신 그 명령을 실행함 (한 칸에 한 번).
