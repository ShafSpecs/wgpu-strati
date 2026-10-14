use alloc::{string::String, vec::Vec};
use core::ffi::CStr;
use std::path::PathBuf;

use crate::auxil::dxgi::result::HResult;
use thiserror::Error;
use windows::{
    core::{Interface, PCSTR, PCWSTR},
    Win32::Graphics::Direct3D::{Dxc, Fxc, ID3DBlob, D3D_SHADER_MACRO},
};

pub(super) enum CompilerContainer {
    Fxc(CompilerFxc),
    DynamicDxc(CompilerDynamicDxc),
    #[cfg_attr(not(static_dxc), allow(unused))]
    StaticDxc(CompilerStaticDxc),
}

pub(super) struct CompilerFxc {
    fxc: FxcLib,
}

pub(super) struct CompilerDynamicDxc {
    max_shader_model: wgt::DxcShaderModel,
    compiler: Dxc::IDxcCompiler3,
    // Has to be held onto for the lifetime of the device otherwise shaders will fail to compile.
    // Only needed when using dynamic linking.
    _dxc: DxcLib,
}

pub(super) struct CompilerStaticDxc {
    max_shader_model: wgt::DxcShaderModel,
    compiler: Dxc::IDxcCompiler3,
}

#[derive(Debug, Error)]
pub(super) enum GetContainerError {
    #[error(transparent)]
    Device(#[from] crate::DeviceError),
    #[error("Failed to load {0}: {1}")]
    FailedToLoad(&'static str, libloading::Error),
}

impl CompilerContainer {
    pub(super) fn new_fxc() -> Result<Self, GetContainerError> {
        FxcLib::new_dynamic().map(|fxc| Self::Fxc(CompilerFxc { fxc }))
    }

    pub(super) fn new_dynamic_dxc(dxc_path: PathBuf) -> Result<Self, GetContainerError> {
        let dxc = DxcLib::new_dynamic(dxc_path)
            .map_err(|e| GetContainerError::FailedToLoad("dxcompiler.dll", e))?;

        let compiler = dxc.create_instance::<Dxc::IDxcCompiler3>()?;

        let (mut major, mut minor) = (1, 0);
        // DXC 1.0 didn't support this. If the cast fails assume it is DXC 1.0.
        if let Ok(version_info) = compiler.cast::<Dxc::IDxcVersionInfo>() {
            unsafe {
                version_info.GetVersion(&mut major, &mut minor).unwrap();
            }
        }

        Ok(Self::DynamicDxc(CompilerDynamicDxc {
            max_shader_model: wgt::DxcShaderModel::from_dxc_version(major, minor),
            compiler,
            _dxc: dxc,
        }))
    }

    /// Creates a [`CompilerContainer`] that delegates to the statically-linked version of DXC.
    pub(super) fn new_static_dxc() -> Result<CompilerContainer, crate::DeviceError> {
        #[cfg(static_dxc)]
        {
            unsafe {
                let compiler = dxc_create_instance::<Dxc::IDxcCompiler3>(|clsid, iid, ppv| {
                    windows_core::HRESULT(mach_dxcompiler_rs::DxcCreateInstance(
                        clsid.cast(),
                        iid.cast(),
                        ppv,
                    ))
                })?;

                Ok(CompilerContainer::StaticDxc(CompilerStaticDxc {
                    max_shader_model: wgt::DxcShaderModel::V6_7,
                    compiler,
                }))
            }
        }
        #[cfg(not(static_dxc))]
        {
            panic!("Attempted to create a static DXC shader compiler, but the static-dxc feature was not enabled")
        }
    }

    pub(super) fn max_shader_model(&self) -> Option<wgt::DxcShaderModel> {
        match self {
            CompilerContainer::Fxc(..) => None,
            CompilerContainer::DynamicDxc(CompilerDynamicDxc {
                max_shader_model, ..
            })
            | CompilerContainer::StaticDxc(CompilerStaticDxc {
                max_shader_model, ..
            }) => Some(max_shader_model.clone()),
        }
    }

    pub(super) fn compile(
        &self,
        device: &super::Device,
        source: &str,
        source_name: Option<&CStr>,
        raw_ep: &str,
        stage_bit: wgt::ShaderStages,
        full_stage: &str,
    ) -> Result<super::CompiledShader, crate::PipelineError> {
        let debug = device
            .shared
            .private_caps
            .instance_flags
            .contains(wgt::InstanceFlags::DEBUG);
        let compiler = match self {
            CompilerContainer::Fxc(_) => "fxc",
            CompilerContainer::DynamicDxc(_) => "dxc",
            CompilerContainer::StaticDxc(_) => "static-dxc",
        };
        let cache = shader_cache::Entry::new(compiler, source, raw_ep, full_stage, debug);
        if let Some(bytes) = cache.as_ref().and_then(shader_cache::Entry::load) {
            return Ok(super::CompiledShader::Precompiled(bytes));
        }
        let compiled = self.compile_uncached(device, source, source_name, raw_ep, stage_bit, full_stage)?;
        if let Some(cache) = cache {
            cache.store(&compiled);
        }
        Ok(compiled)
    }

    fn compile_uncached(
        &self,
        device: &super::Device,
        source: &str,
        source_name: Option<&CStr>,
        raw_ep: &str,
        stage_bit: wgt::ShaderStages,
        full_stage: &str,
    ) -> Result<super::CompiledShader, crate::PipelineError> {
        match self {
            CompilerContainer::Fxc(CompilerFxc { fxc }) => compile_fxc(
                device,
                source,
                source_name,
                raw_ep,
                stage_bit,
                full_stage,
                fxc,
            ),
            CompilerContainer::DynamicDxc(CompilerDynamicDxc { compiler, .. })
            | CompilerContainer::StaticDxc(CompilerStaticDxc { compiler, .. }) => compile_dxc(
                device,
                source,
                source_name,
                raw_ep,
                stage_bit,
                full_stage,
                compiler,
            ),
        }
    }
}

type D3DCompileFn = unsafe extern "system" fn(
    psrcdata: *const core::ffi::c_void,
    srcdatasize: usize,
    psourcename: PCSTR,
    pdefines: *const D3D_SHADER_MACRO,
    pinclude: *mut core::ffi::c_void,
    pentrypoint: PCSTR,
    ptarget: PCSTR,
    flags1: u32,
    flags2: u32,
    ppcode: *mut *mut core::ffi::c_void,
    pperrormsgs: *mut *mut core::ffi::c_void,
) -> windows_core::HRESULT;

#[derive(Debug)]
struct FxcLib {
    // `d3dcompile_fn` points into `_lib`, so `_lib` must be held for as long
    // as we want to keep compiling shaders with FXC.
    _lib: crate::dx12::DynLib,
    d3dcompile_fn: D3DCompileFn,
}

impl FxcLib {
    const PATH: &str = "d3dcompiler_47.dll";

    fn new_dynamic() -> Result<Self, GetContainerError> {
        unsafe {
            let lib = crate::dx12::DynLib::new(Self::PATH)
                .map_err(|e| GetContainerError::FailedToLoad(FxcLib::PATH, e))?;
            let d3dcompile_fn: D3DCompileFn = *lib.get::<D3DCompileFn>(c"D3DCompile".to_bytes())?;

            Ok(Self {
                _lib: lib,
                d3dcompile_fn,
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn compile(
        &self,
        source: &str,
        source_name: Option<&CStr>,
        raw_ep: &str,
        full_stage: &str,
        compile_flags: u32,
        shader_data: &mut Option<ID3DBlob>,
        error: &mut Option<ID3DBlob>,
    ) -> Result<windows_core::Result<()>, crate::DeviceError> {
        unsafe {
            let raw_ep = alloc::ffi::CString::new(raw_ep).unwrap();
            let full_stage = alloc::ffi::CString::new(full_stage).unwrap();

            // If no name has been set, D3DCompile wants the null pointer.
            let source_name = source_name
                .map(|cstr| cstr.as_ptr().cast())
                .unwrap_or(core::ptr::null());

            let shader_data: *mut Option<ID3DBlob> = shader_data;
            let error: *mut Option<ID3DBlob> = error;

            {
                profiling::scope!("Fxc::D3DCompile");
                Ok((self.d3dcompile_fn)(
                    source.as_ptr().cast(),
                    source.len(),
                    PCSTR(source_name),
                    core::ptr::null(),
                    core::ptr::null_mut(),
                    PCSTR(raw_ep.as_ptr().cast()),
                    PCSTR(full_stage.as_ptr().cast()),
                    compile_flags,
                    0,
                    shader_data.cast(),
                    error.cast(),
                )
                .ok())
            }
        }
    }
}

fn compile_fxc(
    device: &super::Device,
    source: &str,
    source_name: Option<&CStr>,
    raw_ep: &str,
    stage_bit: wgt::ShaderStages,
    full_stage: &str,
    fxc: &FxcLib,
) -> Result<super::CompiledShader, crate::PipelineError> {
    profiling::scope!("compile_fxc");
    let mut compile_flags = Fxc::D3DCOMPILE_ENABLE_STRICTNESS;
    if device
        .shared
        .private_caps
        .instance_flags
        .contains(wgt::InstanceFlags::DEBUG)
    {
        compile_flags |= Fxc::D3DCOMPILE_DEBUG | Fxc::D3DCOMPILE_SKIP_OPTIMIZATION;
    }

    let mut shader_data = None;
    let mut error = None;
    let hr = fxc.compile(
        source,
        source_name,
        raw_ep,
        full_stage,
        compile_flags,
        &mut shader_data,
        &mut error,
    )?;

    match hr {
        Ok(()) => {
            let shader_data = shader_data.unwrap();
            Ok(super::CompiledShader::Fxc(shader_data))
        }
        Err(e) => {
            let mut full_msg = format!("FXC D3DCompile error ({e})");
            if let Some(error) = error {
                use core::fmt::Write as _;
                let message = unsafe {
                    core::slice::from_raw_parts(
                        error.GetBufferPointer().cast(),
                        error.GetBufferSize(),
                    )
                };
                let _ = write!(full_msg, ": {}", String::from_utf8_lossy(message));
            }
            Err(crate::PipelineError::Linkage(stage_bit, full_msg))
        }
    }
}

trait DxcObj: Interface {
    const CLSID: windows::core::GUID;
}
impl DxcObj for Dxc::IDxcCompiler3 {
    const CLSID: windows::core::GUID = Dxc::CLSID_DxcCompiler;
}
impl DxcObj for Dxc::IDxcUtils {
    const CLSID: windows::core::GUID = Dxc::CLSID_DxcUtils;
}
impl DxcObj for Dxc::IDxcValidator {
    const CLSID: windows::core::GUID = Dxc::CLSID_DxcValidator;
}

#[derive(Debug)]
struct DxcLib {
    lib: crate::dx12::DynLib,
}

impl DxcLib {
    fn new_dynamic(lib_path: PathBuf) -> Result<Self, libloading::Error> {
        unsafe { crate::dx12::DynLib::new(lib_path).map(|lib| Self { lib }) }
    }

    pub fn create_instance<T: DxcObj>(&self) -> Result<T, crate::DeviceError> {
        unsafe {
            type DxcCreateInstanceFn = unsafe extern "system" fn(
                rclsid: *const windows_core::GUID,
                riid: *const windows_core::GUID,
                ppv: *mut *mut core::ffi::c_void,
            )
                -> windows_core::HRESULT;

            let func: libloading::Symbol<DxcCreateInstanceFn> =
                self.lib.get(c"DxcCreateInstance".to_bytes())?;
            dxc_create_instance::<T>(|clsid, iid, ppv| func(clsid, iid, ppv))
        }
    }
}

/// Invokes the provided library function to create a DXC object.
unsafe fn dxc_create_instance<T: DxcObj>(
    f: impl Fn(
        *const windows_core::GUID,
        *const windows_core::GUID,
        *mut *mut core::ffi::c_void,
    ) -> windows_core::HRESULT,
) -> Result<T, crate::DeviceError> {
    let mut result__ = None;
    f(&T::CLSID, &T::IID, <*mut _>::cast(&mut result__))
        .ok()
        .into_device_result("DxcCreateInstance")?;
    result__.ok_or(crate::DeviceError::Unexpected)
}

/// Owned PCWSTR
#[allow(clippy::upper_case_acronyms)]
struct OPCWSTR {
    inner: Vec<u16>,
}

impl OPCWSTR {
    fn new(s: &str) -> Self {
        let mut inner: Vec<_> = s.encode_utf16().collect();
        inner.push(0);
        Self { inner }
    }

    fn ptr(&self) -> PCWSTR {
        PCWSTR(self.inner.as_ptr())
    }
}

fn get_output<T: Interface>(
    res: &Dxc::IDxcResult,
    kind: Dxc::DXC_OUT_KIND,
) -> Result<T, crate::DeviceError> {
    let mut result__: Option<T> = None;
    unsafe { res.GetOutput::<T>(kind, &mut None, <*mut _>::cast(&mut result__)) }
        .into_device_result("GetOutput")?;
    result__.ok_or(crate::DeviceError::Unexpected)
}

fn as_err_str(blob: &Dxc::IDxcBlobUtf8) -> Result<&str, crate::DeviceError> {
    let ptr = unsafe { blob.GetStringPointer() };
    let len = unsafe { blob.GetStringLength() };
    core::str::from_utf8(unsafe { core::slice::from_raw_parts(ptr.0, len) })
        .map_err(|_| crate::DeviceError::Unexpected)
}

fn compile_dxc(
    device: &crate::dx12::Device,
    source: &str,
    source_name: Option<&CStr>,
    raw_ep: &str,
    stage_bit: wgt::ShaderStages,
    full_stage: &str,
    compiler: &Dxc::IDxcCompiler3,
) -> Result<crate::dx12::CompiledShader, crate::PipelineError> {
    profiling::scope!("compile_dxc");

    let source_name = source_name.and_then(|cstr| cstr.to_str().ok());

    let source_name = source_name.map(OPCWSTR::new);
    let raw_ep = OPCWSTR::new(raw_ep);
    let full_stage = OPCWSTR::new(full_stage);

    let mut compile_args = arrayvec::ArrayVec::<PCWSTR, 13>::new_const();

    if let Some(source_name) = source_name.as_ref() {
        compile_args.push(source_name.ptr())
    }

    compile_args.extend([
        windows::core::w!("-E"),
        raw_ep.ptr(),
        windows::core::w!("-T"),
        full_stage.ptr(),
        windows::core::w!("-HV"),
        windows::core::w!("2018"), // Use HLSL 2018, Naga doesn't supported 2021 yet.
        windows::core::w!("-no-warnings"),
        Dxc::DXC_ARG_ENABLE_STRICTNESS,
    ]);

    if device
        .shared
        .private_caps
        .instance_flags
        .contains(wgt::InstanceFlags::DEBUG)
        && !device
            .shared
            .private_caps
            .workarounds
            .avoid_shader_debug_info
    {
        compile_args.push(Dxc::DXC_ARG_DEBUG);
        compile_args.push(Dxc::DXC_ARG_SKIP_OPTIMIZATIONS);
    }

    if device
        .features
        .intersects(wgt::Features::SHADER_F16 | wgt::Features::SHADER_I16)
    {
        compile_args.push(windows::core::w!("-enable-16bit-types"));
    }

    let buffer = Dxc::DxcBuffer {
        Ptr: source.as_ptr().cast(),
        Size: source.len(),
        Encoding: Dxc::DXC_CP_UTF8.0,
    };

    let compile_res: Dxc::IDxcResult =
        unsafe { compiler.Compile(&buffer, Some(&compile_args), None) }
            .into_device_result("Compile")?;

    drop(compile_args);
    drop(source_name);
    drop(raw_ep);
    drop(full_stage);

    let err_blob = get_output::<Dxc::IDxcBlobUtf8>(&compile_res, Dxc::DXC_OUT_ERRORS)?;

    let len = unsafe { err_blob.GetStringLength() };
    if len != 0 {
        let err = as_err_str(&err_blob)?;
        return Err(crate::PipelineError::Linkage(
            stage_bit,
            format!("DXC compile error: {err}"),
        ));
    }

    let blob = get_output::<Dxc::IDxcBlob>(&compile_res, Dxc::DXC_OUT_OBJECT)?;

    Ok(crate::dx12::CompiledShader::Dxc(blob))
}

/// An on-disk cache of compiled shaders, enabled by `WGPU_DX12_SHADER_CACHE`
/// (a directory). Compiling HLSL with FXC or DXC dominates pipeline creation
/// on D3D12 (hundreds of milliseconds for a renderer's pipelines at startup),
/// and the output depends only on what goes in: the compiler, the HLSL, the
/// entry point, the target profile and the flags. Entries are keyed by a hash
/// of those and this crate's version, written to a temporary file and renamed
/// (concurrent processes can't leave a partial entry), and used only if they
/// are a complete DXBC container (the format both compilers produce), so a
/// damaged entry is compiled again rather than failing pipeline creation.
mod shader_cache {
    use alloc::{format, string::String, vec::Vec};
    use std::path::PathBuf;

    pub(super) struct Entry {
        path: PathBuf,
    }

    impl Entry {
        pub(super) fn new(compiler: &str, source: &str, entry: &str, profile: &str, debug: bool) -> Option<Self> {
            let dir = PathBuf::from(std::env::var_os("WGPU_DX12_SHADER_CACHE")?);
            let mut key = Fnv128::default();
            for part in [env!("CARGO_PKG_VERSION"), compiler, entry, profile, if debug { "debug" } else { "release" }, source] {
                key.write(part.as_bytes());
                key.write(&[0xff]);
            }
            Some(Self { path: dir.join(format!("{}.dxbc", key.hex())) })
        }

        pub(super) fn load(&self) -> Option<Vec<u8>> {
            let bytes = std::fs::read(&self.path).ok()?;
            is_container(&bytes).then_some(bytes)
        }

        pub(super) fn store(&self, compiled: &super::super::CompiledShader) {
            let code = compiled.create_native_shader();
            // SAFETY: the bytecode belongs to `compiled`, alive for this call.
            let bytes = unsafe { core::slice::from_raw_parts(code.pShaderBytecode.cast::<u8>(), code.BytecodeLength) };
            if !is_container(bytes) {
                return;
            }
            let Some(dir) = self.path.parent() else { return };
            if std::fs::create_dir_all(dir).is_err() {
                return;
            }
            let tmp = self.path.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &self.path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    /// A DXBC container ("DXBC", a 16-byte checksum, a version, then its
    /// total size), whole.
    fn is_container(bytes: &[u8]) -> bool {
        bytes.len() >= 32
            && &bytes[..4] == b"DXBC"
            && u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]) as usize == bytes.len()
    }

    /// FNV-1a, 128-bit: stable across builds and platforms (unlike std's
    /// hashers), and no dependency.
    struct Fnv128(u128);

    impl Default for Fnv128 {
        fn default() -> Self {
            Self(0x6c62272e07bb014262b821756295c58d)
        }
    }

    impl Fnv128 {
        fn write(&mut self, bytes: &[u8]) {
            for &b in bytes {
                self.0 ^= u128::from(b);
                self.0 = self.0.wrapping_mul(0x0000000001000000000000000000013b);
            }
        }

        fn hex(&self) -> String {
            format!("{:032x}", self.0)
        }
    }
}
