# image-viewer

Rust로 만드는 빠른 Windows 이미지 뷰어.

- **속도 우선**: 저해상도 미리보기 먼저 표시, 나선형 미리 읽기, GC 없는 일정한 반응성
- **다양한 형식**: JPEG, PNG, WebP, GIF, TIFF, AVIF, HEIC, JPEG XL, RAW, SVG 등
- **간단한 편집**: 회전, 뒤집기, 자르기, 크기 변경, 기본 보정, 실행 취소/다시 실행
- **형식 변환**: 다른 이름으로 저장, 일괄 변환, 잘못된 확장자 바로잡기

## 상태

M4(편집·변환) 완료: 회전·뒤집기·자르기·실행 취소, 저장/다른 이름으로 저장(JPEG 회전은 무손실), 9개 형식 인코더, 일괄 변환 CLI, 확장자 바로잡기.
M5(확장)에서: 보정·크기 변경 UI, 압축파일 감상, 두 쪽 보기 등.

설계와 개발 단계는 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) 참고.

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
./target/release/image-viewer <파일>    # 뷰어 (--trace: 시작 단계별 시간 출력)
```

### 기본 단축키

| 키 | 동작 |
|---|---|
| Ctrl+O | 파일 열기 |
| →, Space, PageDown / ←, Backspace, PageUp | 다음 / 이전 (누르고 있으면 빠른 넘김) |
| Home / End | 처음 / 마지막 |
| R, Ctrl+R / L, Ctrl+L | 시계 / 반시계 방향 회전 |
| H / V | 좌우 / 상하 뒤집기 |
| Shift+드래그, C 또는 Enter | 영역 선택, 선택 영역으로 자르기 |
| Ctrl+Z / Ctrl+Y | 실행 취소 / 다시 실행 |
| Ctrl+S / Ctrl+Shift+S | 저장 / 다른 이름으로 저장 |
| F, Ctrl+0 | 창에 맞춤 |
| 1, Ctrl+1 | 100% |
| +, =, Ctrl++ / -, Ctrl+- | 확대 / 축소 |
| 휠 | 커서 기준 확대·축소 |
| 드래그 | 이동 |
| 더블클릭 | 맞춤 ↔ 100% |
| F11, Alt+Enter | 전체 화면 |
| Esc | 선택 해제 → 전체 화면 해제 → 종료 |
| Ctrl+W, Ctrl+Q | 종료 |

`%APPDATA%\image-viewer\keymap.toml`로 바꿀 수 있음:

```toml
[bindings]
"Q" = "app.quit"
"F" = ""          # 기본 바인딩 해제
```
