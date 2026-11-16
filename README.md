# image-viewer

Rust로 만드는 작고 빠른 Windows 이미지 뷰어. GPU 드라이버도 UI 툴킷도 쓰지 않는 순수 Win32 창: 사진 한 장 띄운 상태 메모리 약 43 MB, 실행 후 50 ms 안에 사진 표시.

- **속도 우선**: 저해상도 미리보기 먼저 표시, 나선형 미리 읽기, GC 없는 일정한 반응성
- **다양한 형식**: JPEG, PNG, WebP, GIF, TIFF, AVIF, HEIC, JPEG XL, RAW, SVG 등
- **간단한 편집**: 회전, 뒤집기, 자르기, 크기 변경, 기본 보정, 실행 취소/다시 실행
- **형식 변환**: 다른 이름으로 저장, 일괄 변환, 잘못된 확장자 바로잡기

## 상태

M0~M6 완료:

- 빠른 보기: CPU 렌더러(보이는 부분만 SIMD 리샘플, GDI로 표시), 미리보기 먼저, 방향 고려 미리 읽기, 키를 누르고 있을 때의 빠른 넘김
- 형식: JPEG, PNG/APNG, GIF, WebP, AVIF, HEIC, JPEG XL, TIFF, BMP, ICO, QOI, TGA, PNM, DDS, EXR, HDR, SVG, 카메라 RAW(Windows 코덱), ICC 색 관리
- 압축파일: ZIP/CBZ, 7z/CB7 (만화 두 쪽 보기)
- 편집: 회전·뒤집기·자르기·보정·크기 변경, 실행 취소, 저장/다른 이름으로 저장 (JPEG 회전은 무손실)
- 변환: 9개 형식 인코더, 일괄 변환 CLI, 확장자 바로잡기

설계와 개발 단계는 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), 개발 규칙(빌드 캐시 용량 관리 포함)은 [AGENTS.md](AGENTS.md) 참고.

## 빌드

필요: Rust stable (MSVC 툴체인), Visual Studio 2022 Build Tools (C++ 워크로드), NASM (AV1 디코더 `rav1d`의 어셈블리 최적화용).

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
./target/release/iv fix-ext --dry-run <파일|폴더>   # 내용과 다른 확장자 찾기 (빼면 실제로 바꿈)
./target/release/image-viewer <파일|폴더>    # 뷰어 (--trace: 시작 단계별 시간 출력, --no-activate: 포커스를 가져가지 않고 열기)
```

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
| Esc | 선택 해제 → 전체 화면 해제 → 종료 |
| Ctrl+W, Ctrl+Q | 종료 |

한글 입력 상태(IME 한글 모드)여도 R·F·D 같은 한 글자 단축키가 그대로 동작함 (뷰어 창에서는 입력기를 끔).

저장(Ctrl+S)은 파일 내용의 실제 형식으로 씀: 이름이 `.jpg`인 WebP는 WebP 그대로(투명도·무손실 유지). 제목 표시줄에 "확장자가 내용과 다름"이 보이면 Ctrl+E로 이름을 바로잡을 수 있음. 움직이는 이미지(GIF·WebP·APNG, 첫 프레임만 저장되므로 원본 보호), 압축파일 안의 페이지, 쓸 수 없는 형식(HEIC·RAW 등)은 그 자리에 저장하지 않고 이유를 보여 준 뒤 다른 이름으로 저장을 엶. 저장하는 동안에는 편집·실행 취소·두 번째 저장·이름 바꾸기를 받지 않고, 다음·이전·열기·종료는 저장이 끝난 뒤 이어서 진행함 (저장이 실패하면 진행하지 않고 편집은 남음).

설정은 F2 또는 `%APPDATA%\image-viewer\settings.toml` (`--settings <파일>`로 다른 파일 지정).
설정 파일이나 키맵 파일을 읽지 못하면 시작할 때 제목 표시줄에 이유(파일·줄 번호)가 첫 명령 전까지 표시됨. 읽지 못한 설정 파일은 설정 창에서 저장할 때 `settings.toml.bak`(있으면 `.bak2`, …)으로 보관한 뒤 새로 씀. 설정 파일은 임시 파일에 쓴 뒤 바꿔치기하므로 저장 도중 꺼져도 망가지지 않음.
창 위치·크기·최대화 상태는 닫을 때 설정 파일 옆의 `window.toml`에 따로 기록되어 다음 실행 때 그대로 열림 (`settings.toml`은 설정 창에서 저장할 때만 쓰므로 직접 고친 값·주석은 그대로 남음) (전체 화면은 기억하지 않음, 연결이 끊긴 모니터 위치는 무시). 처음 열 때는 주 모니터 배율에 맞춘 1280×800(논리 픽셀) 크기로 가운데에 열림.
화면 언어는 한국어가 기본이고 설정(F2)의 "언어"에서 English로 바꿀 수 있음 (바로 적용, `settings.toml`의 `language = "ko"` / `"en"`).

단축키는 `%APPDATA%\image-viewer\keymap.toml`로 바꿀 수 있음 (`--keymap <파일>`로 다른 파일 지정):

```toml
[bindings]
"Q" = "app.quit"
"F" = ""          # 기본 바인딩 해제
"Ctrl+WheelDown" = "nav.next"   # 마우스: WheelUp, WheelDown, Mouse4(뒤로), Mouse5(앞으로)
```

키맵에서 휠에 명령을 묶으면 그 조합의 휠은 확대 대신 그 명령을 실행함 (한 칸에 한 번).
