# image-viewer

Rust로 만드는 빠른 Windows 이미지 뷰어.

- **속도 우선**: 저해상도 미리보기 먼저 표시, 나선형 미리 읽기, GC 없는 일정한 반응성
- **다양한 형식**: JPEG, PNG, WebP, GIF, TIFF, AVIF, HEIC, JPEG XL, RAW, SVG 등
- **간단한 편집**: 회전, 뒤집기, 자르기, 크기 변경, 기본 보정, 실행 취소/다시 실행
- **형식 변환**: 다른 이름으로 저장, 일괄 변환, 잘못된 확장자 바로잡기

## 상태

M0(기반) 완료: 코어 타입, 형식 판별, 디코더 레지스트리, `iv` CLI. 뷰어 창은 M1에서.

설계와 개발 단계는 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) 참고.

## 빌드

필요: Rust stable (MSVC 툴체인), Visual Studio 2022 Build Tools (C++ 워크로드).

```bash
cargo test
cargo build --release
./target/release/iv formats            # 형식별 등록 디코더
./target/release/iv probe <파일|폴더>   # 형식·크기·방향, 확장자 불일치 표시
./target/release/iv bench <파일|폴더>   # probe / preview / decode 시간 측정
```
