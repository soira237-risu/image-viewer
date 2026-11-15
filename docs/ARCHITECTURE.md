# Image Viewer — 설계 및 개발 계획

작성일: 2026-11-14 · 언어: Rust · 대상: Windows 10/11 x64 (코어는 플랫폼 독립 유지)

## 0. 목표와 우선순위

1. **극도의 속도**: 실행 즉시 표시, 넘김 시 끊김 없음, 일정한 프레임 (GC 없음)
2. **다양한 형식**: JPEG/PNG/WebP/GIF/BMP/TIFF/AVIF/HEIC/JXL/RAW/SVG/ICO 등
3. **간단한 편집**: 회전·뒤집기·자르기·크기 변경·밝기/대비 등 기본 보정, 실행 취소/다시 실행
4. **형식 변환(확장자 변경)**: 다른 이름으로 저장, 일괄 변환, 잘못된 확장자 바로잡기
5. **확장성**: 코덱·편집 연산·명령을 기존 코드 수정 없이 추가할 수 있는 구조

비목표(초기): 레이어 편집, 그리기/텍스트 주석, 동영상, 클라우드 동기화.

### 참고: ImageGlass v10 분석에서 가져온 교훈

- 빠른 디코더(Skia) 우선 → 만능 디코더(Magick) 후순위의 **2단 코덱 구조**
- 원본 디코드 전에 **저해상도 미리보기 먼저 표시** (JPEG IDCT 축소, EXIF/RAW 내장 미리보기)
- **나선형 미리 읽기** (±1, ±2 …), 메모리 예산, 탐색 시 즉시 취소, 키 누르고 있는 동안 미리 읽기 중단
- 8192² 초과 이미지는 **타일 + 밉맵**
- 실제 사고 사례: 캐시 추적 누락으로 3.6GB 누수, 디코드 취소 중 이중 해제 → Rust 소유권 모델과 명확한 캐시 소유 구조로 예방
- ImageGlass는 GPLv3: 구조만 참고, 코드 복사 금지

## 1. 환경 현황 (2026-11-14 확인)

| 항목 | 상태 |
|---|---|
| 프로젝트 폴더 | 비어 있음, git 저장소 아님 |
| Rust (rustup/cargo) | **미설치** |
| MSVC Build Tools | **미설치** (Rust MSVC 타깃 링크에 필요) |
| cmake / nasm / vcpkg | 미설치 → 초기에는 C 의존성 회피 |
| WIC 확장 코덱 | HEIF 1.2.48, HEVC 2.4.45, AV1 2.0.24, RAW 2.5.24, WebP 1.2.31 설치됨 |

결론: 초기 단계는 **순수 Rust 크레이트 + Windows WIC** 조합으로 C 빌드 체인 없이 진행.
libjpeg-turbo, libheif, libjxl 등 C/C++ 의존성은 이후 cargo feature로 선택 추가.

## 2. 전체 구조

```
┌────────────────────────────── iv-app (winit + wgpu + egui) ───────────────────────────┐
│  입력 → Keymap → Command ─┬─> Navigator ──> Library(목록·정렬) ──> Prefetcher(나선형) │
│                           ├─> EditSession(EditStack, undo/redo)                        │
│                           └─> Exporter(저장·변환)                                       │
│  Renderer(wgpu 텍스처·확대/이동·타일) <── DecodeService(작업자 스레드·세대 취소)        │
└────────────────────────────────────────────────────────────────────────────────────────┘
          │                         │                       │
     iv-render                 iv-pipeline              iv-ops
          │                         │                       │
          └──────────── iv-codec (Codec 트레이트 + Registry) ┘
                                    │
                                 iv-core (ImageBuf, Metadata, Source, Error, CancelToken)

iv-cli : iv-pipeline + iv-ops + iv-codec 재사용 (일괄 변환·벤치마크·테스트용)
```

### Cargo workspace 구성

| 크레이트 | 역할 | UI 의존 |
|---|---|---|
| `iv-core` | 픽셀 버퍼, 메타데이터, 입력 소스, 오류, 취소 토큰 | 없음 |
| `iv-codec` | 코덱 트레이트, 레지스트리, 형식 판별, 내장 코덱(feature별) | 없음 |
| `iv-ops` | 편집 연산, EditStack, 무손실 변환 판정 | 없음 |
| `iv-pipeline` | 폴더 목록·자연 정렬, 디코드 서비스, 캐시, 미리 읽기, 내보내기 | 없음 |
| `iv-render` | wgpu 텍스처 업로드, 확대/이동 변환, 타일·밉맵 | wgpu |
| `iv-app` | 창, 입력, 명령, 설정, egui 오버레이/대화상자 | 전부 |
| `iv-cli` | `iv convert`, `iv bench`, `iv probe` | 없음 |

UI 없는 크레이트가 핵심 로직 전부를 담아 **CLI·테스트·벤치마크에서 그대로 검증**하는 것이 원칙.

## 3. 핵심 추상화 (확장 지점)

### 3.1 입력 소스 — `Source`

처음부터 "파일"이 아니라 `Source`로 추상화. 이후 압축파일(zip/7z/rar) 내부 이미지, 클립보드, 드래그 데이터가 같은 경로로 들어옴.

```rust
pub enum SourceKind { File(PathBuf), ArchiveEntry { archive: PathBuf, entry: String }, Memory(Arc<[u8]>) }

pub struct Source {
    pub kind: SourceKind,
    pub display_name: String,
    // 앞부분 바이트(형식 판별용)와 전체 바이트 접근은 지연 로딩 (파일은 mmap)
}
```

### 3.2 픽셀 버퍼 — `ImageBuf`

```rust
pub enum PixelFormat { Rgba8, Rgba16, RgbaF16, RgbaF32 }   // 8비트 경로 최적화, HDR/16비트 보존
pub struct ImageBuf {
    pub width: u32, pub height: u32,
    pub format: PixelFormat,
    pub color: ColorInfo,          // sRGB / ICC 프로파일 / 전달 함수(PQ, HLG)
    pub data: Arc<[u8]>,           // 캐시·렌더러·편집이 복사 없이 공유
}
```

### 3.3 코덱 — `Codec`, `Decoder`, `Encoder`

```rust
pub trait Codec: Send + Sync {
    fn info(&self) -> &CodecInfo;                 // id, 이름, 확장자, 디코드/인코드 우선순위, 능력 플래그
    fn sniff(&self, head: &[u8]) -> bool;         // 매직 바이트 판별 (확장자보다 우선)
    fn decoder(&self) -> Option<&dyn Decoder> { None }
    fn encoder(&self) -> Option<&dyn Encoder> { None }
    fn lossless(&self) -> Option<&dyn LosslessTransformer> { None }  // JPEG 무손실 회전/자르기
}

pub trait Decoder: Send + Sync {
    /// 픽셀 디코드 없이 크기·프레임 수·방향·ICC만 읽음 (목록 정렬, 캐시 판단, 미리보기 크기 계산)
    fn probe(&self, src: &Source) -> Result<ImageInfo>;
    /// 저해상도 미리보기: JPEG 축소 디코드, EXIF/RAW 내장 썸네일 등. 없으면 None
    fn preview(&self, src: &Source, max_side: u32, cancel: &CancelToken) -> Result<Option<ImageBuf>>;
    /// 원본 디코드. req에 프레임 번호·목표 크기(축소 디코드)를 담음
    fn decode(&self, src: &Source, req: &DecodeRequest, cancel: &CancelToken) -> Result<Decoded>;
}

pub enum Decoded { Still(ImageBuf), Animated(Box<dyn FrameStream>) }

pub trait Encoder: Send + Sync {
    /// 품질·무손실·속도 등 옵션을 데이터로 선언 → 저장 대화상자와 CLI가 자동 생성
    fn options(&self) -> &[OptionSpec];
    fn accepts(&self, fmt: PixelFormat, has_alpha: bool) -> Acceptance; // 알파/비트 깊이 지원 여부
    fn encode(&self, img: &ImageBuf, meta: &Metadata, opts: &OptionValues, out: &mut dyn Write) -> Result<()>;
}
```

**레지스트리 선택 규칙**
1. 매직 바이트로 형식 판별 → 확장자는 보조 단서 (확장자와 실제 형식이 다르면 "확장자 바로잡기" 제안)
2. 해당 형식을 지원하는 코덱 중 우선순위가 높은 것부터 시도
3. 실패하면 다음 코덱으로 내려감 (최종 보루: WIC)
4. 형식별 승자는 캐시, 코덱 등록/해제 시 무효화

**코덱 추가 방법**: 모듈 하나 작성 → `Codec` 구현 → cargo feature 추가 → 레지스트리에 등록 한 줄.
런타임 플러그인(DLL)은 후순위. 필요 시 `extern "C"` ABI 어댑터가 같은 `Codec` 트레이트를 구현하는 방식으로 추가 (Rust는 안정 ABI가 없으므로 C ABI 경계 필수).

### 3.4 편집 연산 — `Op`, `EditStack`

```rust
#[derive(Clone, Serialize, Deserialize)]
pub enum Op {
    Rotate90(Turns), FlipH, FlipV,                 // 직교 변환: JPEG 무손실 경로 후보
    Crop(Rect),                                    // MCU 경계 정렬 시 무손실 후보
    Resize { w: u32, h: u32, filter: Filter },
    Adjust { brightness: f32, contrast: f32, saturation: f32, gamma: f32 },
    AutoOrient,                                    // EXIF 방향 적용
    // 확장: 새 변형 + apply 구현 추가. 런타임 플러그인용 Custom(Arc<dyn Operation>)는 이후
}

pub struct EditStack { ops: Vec<Op>, cursor: usize }  // undo = cursor-1, redo = cursor+1
```

- **비파괴 편집**: 원본 버퍼는 그대로 두고 연산 목록만 쌓음
- **화면 미리보기**는 화면 해상도로 축소한 버퍼에 연산 적용 → 큰 사진도 즉시 반응
- **저장 시**에만 원본 해상도로 연산 적용
- 직교 변환/자르기는 가능하면 **GPU 변환 행렬**로 표시만 바꾸고 픽셀 계산은 저장 시점으로 미룸

### 3.5 내보내기 — `Exporter` (저장·형식 변환의 단일 경로)

GUI의 저장/다른 이름으로 저장, 일괄 변환, `iv convert`가 모두 같은 함수를 사용.

```
export(source, edits, target_codec, options, dest):
  1. 무손실 경로 판정: 원본 JPEG → 대상 JPEG, 연산이 직교 변환(+MCU 정렬 자르기)뿐
     → LosslessTransformer 사용 (재압축 없음)
  2. 일반 경로: 원본 해상도 디코드 → 연산 적용 → 대상 형식에 맞게 변환
       - 알파 → JPEG: 배경색과 합성
       - 16비트/HDR → 8비트 형식: 톤매핑 + 디더링
       - 애니메이션 → 정지 형식: 현재 프레임 또는 거부(명시적 선택)
     → 인코드
  3. 메타데이터 정책: EXIF/ICC/XMP 유지(옵션), 방향 태그=1로 재설정, 크기 갱신, 내장 썸네일 제거
  4. 안전한 쓰기: 같은 폴더 임시 파일에 쓰기 → flush/sync → 원자적 교체(rename/ReplaceFileW)
     → 실패 시 원본 무손상 보장, 덮어쓰기 전 확인
```

"확장자 변경"의 두 가지 의미를 모두 지원:
- **형식 변환**: PNG → WebP 등 실제 재인코딩 (위 경로)
- **확장자 바로잡기**: 내용은 WebP인데 이름이 `.jpg`인 경우 재인코딩 없이 이름만 변경

### 3.6 명령 시스템 — `Command`

모든 동작(다음/이전, 확대, 회전, 자르기, 저장, 변환, 휴지통 삭제 …)을 문자열 id를 가진 명령으로 정의.
키 설정(TOML), 메뉴, 툴바, 이후 스크립트/플러그인이 모두 같은 명령을 호출 → 기능 추가 시 UI 배선 최소화.

```toml
# keymap.toml
"Right"      = "nav.next"
"Left"       = "nav.prev"
"Ctrl+R"     = "edit.rotate_cw"
"Ctrl+S"     = "file.save"
"Ctrl+Shift+S" = "file.save_as"
```

## 4. 속도 설계

| 단계 | 기법 |
|---|---|
| 실행 | 단일 네이티브 exe, 창 먼저 띄우고 디코드는 병렬 시작, 설정 로드는 최소화 |
| 첫 표시 | `preview()`로 저해상도 먼저 → 원본 완료 시 교체 |
| 디코드 | 형식별 최속 디코더 (zune-jpeg 등), 화면보다 훨씬 큰 JPEG는 축소 디코드 |
| 넘김 | 나선형 미리 읽기, 메모리 예산(기본 1GB), 장수 제한, 탐색 시 세대 번호로 낡은 작업 폐기 |
| 연속 넘김 | 키를 누르고 있는 동안 미리 읽기 중단, 미리보기만 표시 |
| 업로드 | 디코드 결과를 작업자 스레드에서 GPU 업로드 준비, UI 스레드는 그리기만 |
| 거대 이미지 | 512px 타일 + 밉맵 + LRU, 그동안 축소 대리 이미지 표시 |
| 스레드 | UI 스레드는 디코드 금지. 결과는 채널 + `EventLoopProxy`로 깨움 |

**취소 모델**: 탐색마다 세대 번호(AtomicU64) 증가. 디코더는 단계 사이에서 `CancelToken` 확인, 끝까지 진행된 낡은 결과는 캐시 정책에 따라 보관 또는 폐기. 캐시가 디코드 결과의 **유일한 소유자** (ImageGlass의 추적 누락 누수 방지).

**측정 우선**: M0에서 `iv bench`로 형식별 probe/preview/decode 시간을 측정하고, 이후 모든 최적화는 측정치로 판단.

| 목표치 (측정 전 가설) | 값 |
|---|---|
| 캐시된 이미지로 넘김 → 화면 표시 | 1프레임(16ms) 이내 |
| 24MP JPEG 미리보기 표시 (콜드) | 50ms 이내 |
| 실행 → 첫 이미지 미리보기 | 150ms 이내 |

## 5. 형식 지원 계획 (크레이트 버전은 2026-11-14 crates.io 기준)

| 형식 | 디코드 (1순위 → 보루) | 인코드 | 비고 |
|---|---|---|---|
| JPEG | `zune-jpeg` 0.5 (`image` 0.25도 내부적으로 사용) → WIC | `image` 내장 → 이후 `turbojpeg`/mozjpeg | 무손실 회전: `turbojpeg` 1.5 (C 빌드 필요, M4 이후) |
| PNG | `image` 0.25 (`png`) | `image` + `oxipng` 10 최적화 | |
| WebP | `image-webp` 0.2 → WIC | `image-webp`(무손실) / `webp` 0.3(손실, libwebp) | |
| GIF | `image` (`gif`) | `image` | 애니메이션 |
| BMP/ICO/TGA/PNM/QOI/DDS | `image` | `image` | |
| TIFF | `image` (`tiff`) → WIC | `image` | |
| AVIF | **WIC (AV1 확장)** → 이후 dav1d | `ravif` 0.13 | |
| HEIC/HEIF | **WIC (HEIF 확장)** → 이후 `libheif-rs` 3.0 | (보류) | libheif는 LGPL |
| JPEG XL | `jxl-oxide` 0.12 (순수 Rust) | (보류, libjxl 필요) | |
| RAW | 내장 미리보기 우선 → WIC(RAW 확장) → `rawler` 0.8 | 해당 없음 | |
| SVG | `resvg` 0.50 | 해당 없음 | |
| EXR/HDR | `image` | `image` | HDR 톤매핑은 M5 |
| PSD | (M5 이후 검토) | 해당 없음 | |

**WIC 보루 코덱**: `windows` 0.62 크레이트로 `IWICImagingFactory` 사용. OS에 설치된 코덱을 그대로 활용하므로 형식 범위가 넓고 하드웨어 가속 혜택도 있음. Windows 전용이므로 `cfg(windows)` 뒤에 격리.

**기타 크레이트 후보**

| 용도 | 크레이트 |
|---|---|
| 창/입력 | `winit` 0.30 |
| GPU 렌더링 | `wgpu` 30 |
| UI 오버레이·대화상자 | `egui` / `egui-wgpu` 0.37 |
| 리사이즈 (SIMD) | `fast_image_resize` 6.1 |
| 병렬 처리 | `rayon` 1.12 |
| EXIF 읽기 | `kamadak-exif` 0.6 |
| 메타데이터 이식 (JPEG/PNG/WebP 청크) | `img-parts` 0.4 |
| 색 관리 | `moxcms` 0.9 (순수 Rust, `image`도 내부 사용) vs `lcms2` 6.2 (C 빌드) — M3에서 결정 |
| 폴더 감시 | `notify` 8.2 |
| 파일 대화상자 | `rfd` 0.18 |
| 휴지통 | `trash` 5.3 |
| 클립보드 | `arboard` 3.7 |
| 압축파일 | `zip` 9.5, `sevenz-rust2` 0.25, `unrar` 0.5 (unRAR 라이선스 확인 필요) |

## 6. 개발 단계 (각 단계마다 측정·테스트로 완료 판정)

| 단계 | 내용 | 완료 기준 |
|---|---|---|
| **M0 기반** | rustup + MSVC Build Tools 설치, workspace 골격, `iv-core`/`iv-codec` 기본, `iv probe`/`iv bench` | 샘플 폴더에서 형식별 디코드 시간 표 출력, `cargo test` 통과 |
| **M1 보기** | winit+wgpu 창, 파일 1장 표시, 창 맞춤/확대/이동, 전체화면, 명령·키맵 | 명령줄 인자로 연 JPEG/PNG/WebP/GIF가 정확히 표시됨 |
| **M2 넘김** | 폴더 목록·자연 정렬, 미리보기 먼저, 나선형 미리 읽기, 세대 취소, 연속 넘김 모드 | 벤치마크로 넘김 지연 측정, 메모리 예산 초과 없음 확인 |
| **M3 형식** | WIC 보루(HEIC/AVIF/RAW), JXL, SVG, 애니메이션, EXIF 방향, ICC | 형식별 샘플 코퍼스 전부 열림, 회귀 테스트 |
| **M4 편집·변환** | EditStack(회전·뒤집기·자르기·크기·보정), undo/redo, 저장/다른 이름으로 저장, 옵션 자동 UI, 안전한 쓰기, 메타데이터 정책, `iv convert` | 왕복 테스트(PSNR 기준), 원본 무손상 테스트, 연산 골든 테스트 |
| **M5 확장** | 일괄 변환, 확장자 바로잡기, 압축파일 감상, 두 쪽 보기(만화), 거대 이미지 타일, 설정 화면 | 기능별 통합 테스트 |
| 이후 | JPEG 무손실 변환(turbojpeg), HDR 톤매핑, 런타임 플러그인 ABI, 탐색기 정렬 순서 연동 | |

### M0 결과 (2026-11-15)

- 툴체인: rustc 1.100.0 (stable-x86_64-pc-windows-msvc), VS 2022 Build Tools (VC 도구 + Windows SDK)
- 구현: `iv-core`(오류·취소·형식·`ImageBuf`·`Source`), `iv-codec`(형식 판별, `Decoder` 트레이트, `Registry` 우선순위·폴백, zune-jpeg·image-rs 디코더, EXIF 방향·내장 썸네일 미리보기), `iv-cli`(`probe`, `bench`, `formats`)
- 테스트 28개: 무손실 형식 왕복 픽셀 일치, JPEG 오차, 16비트 보존, 확장자 불일치 판별, 손상 파일, 취소, 폴백 체인, EXIF 방향·썸네일
- 측정 (release, 3840×2400 = 9.2MP, 중앙값 7회): JPEG 34.8ms (314 MP/s), TIFF 36.5ms, QOI 59.6ms, BMP 101ms, WebP(무손실) 103ms, PNG 131ms. probe는 모든 형식 0.2ms 이하
- 측정에서 얻은 결정
  - `ImageBuf`를 `Arc<Vec<u8>>`로 변경: `Vec → Arc<[u8]>` 변환이 전체 버퍼를 복사하던 낭비 제거 (JPEG 42.6 → 34.8ms)
  - Windows 배경화면 JPEG에는 EXIF 썸네일이 없음 → 미리보기 경로가 비어 있음. 화면 크기 축소 디코드(libjpeg-turbo IDCT 축소 또는 GPU 축소 표시)는 M2에서 측정 후 결정
  - BMP가 원시 형식임에도 느림(image-rs의 행 단위 처리) → 빈도 낮아 보류, 필요 시 전용 디코더
- 벤치 코퍼스: `cargo run -p iv-codec --release --example make_corpus -- C:/Windows/Web/Wallpaper/ThemeC testdata/local/corpus 4`

### M1 결과 (2026-11-15)

- 구현
  - `iv-render`: `ViewState`(창 맞춤·커서 기준 확대·이동 제한), EXIF 방향 → UV 변환, sRGB 텍스처 + GPU 밉맵(선형광 평균), 체커보드 알파 합성, 3배 초과 확대 시 nearest, GPU 한계 초과 이미지는 Lanczos3 축소
  - `iv-app`: winit 0.30 창, 명령(`file.open`, `view.*`, `app.*`) + `%APPDATA%\image-viewer\keymap.toml` 재정의, 휠 확대·드래그 이동·더블클릭 맞춤/100%, 전체 화면, 드래그 앤 드롭, 파일 열기 대화상자, `--trace` 시작 계측
- 시작 경로: 프로세스 시작 즉시 GPU 장치 생성 스레드와 디코드 스레드가 동시에 출발 → 창 생성과 겹침
- 측정 (release, 6400×4000 JPEG, RX 9060, 5회 중앙값, 바이너리 첫 실행 제외)
  - 백엔드 비교: 첫 전체 화면 Vulkan 380ms vs DX12 326ms → Windows는 DX12 우선(`WGPU_BACKEND`로 재정의 가능)
  - GPU 초기화를 시작 직후로 이동한 뒤: GPU 준비 115ms, **첫 전체 화면 221ms**
  - 내역: 디코드 67ms(GPU 준비와 병렬), 업로드+밉맵 40ms(102MB), 첫 present 약 16ms
  - 바이너리를 새로 빌드한 직후 첫 실행은 드라이버 셰이더 캐시 생성으로 600~800ms까지 늘어남
- 확인: 오프스크린 렌더 픽셀 검사 테스트(위치·방향 4종·체커보드·밉맵·16비트), 실제 창 캡처로 맞춤 표시·EXIF 6 회전·알파·키 확대 확인
- 미확인: 실제 키보드로 같은 키를 연타했을 때의 동작(합성 키 메시지로는 3회 중 1회만 반영됨. 하네스 한계로 추정)
- 남은 개선 후보: 업로드 40ms → 화면 크기 축소본 먼저 업로드 후 원본 교체(M2에서 측정)

## 7. 테스트 전략

- **코덱**: 형식별 샘플 코퍼스(`testdata/`), probe 결과·크기·프레임 수 검증, 손상 파일에서 패닉 없이 오류 반환
- **인코더 왕복**: `decode(encode(x))`와 `x` 비교 — 무손실은 완전 일치, 손실은 PSNR 하한
- **편집 연산**: 작은 합성 이미지에 대한 골든 픽셀 테스트, 연산 조합 교환 법칙(회전 4번 = 원본 등) 속성 테스트
- **내보내기**: 쓰기 도중 실패 주입 시 원본 무손상, 메타데이터 보존·방향 태그 재설정 확인
- **성능**: `iv bench` 결과를 단계별로 기록해 회귀 감시
- **동시성**: 빠른 연속 탐색 시뮬레이션 테스트에서 캐시 메모리 상한·누수 없음 확인

## 8. 열린 결정 사항

- 색 관리: `moxcms`(순수 Rust, C 빌드 불필요) vs `lcms2`(검증된 C 라이브러리) — M3에서 정확도·속도 측정 후 결정
- JPEG 디코더: `zune-jpeg` vs libjpeg-turbo — M0 벤치마크로 결정
- RAR 지원 여부: unRAR 라이선스 조건 검토 후 결정
- 런타임 플러그인 필요 여부: M5 이후 실제 수요로 판단
