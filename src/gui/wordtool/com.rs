//! IDispatch 晚期绑定封装：等价 C# 的 `Type.GetTypeFromProgID + Activator.CreateInstance + dynamic`。
//! 所有 VARIANT 的构造/读取/清理集中在本模块，杜绝泄漏（等价 C# 的 Marshal.ReleaseComObject + GC 范式，
//! 但引用计数由 Drop 即时释放，比 RCW 等待 GC 更精确）。

use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, CLSIDFromProgID, COINIT_APARTMENTTHREADED, CoCreateInstance,
    CoInitializeEx, CoUninitialize, DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET,
    DISPATCH_PROPERTYPUT, DISPPARAMS, EXCEPINFO, IDispatch,
};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0_0, VT_BOOL, VT_BSTR, VT_DISPATCH, VT_I2, VT_I4, VT_R4, VT_R8, VT_UNKNOWN,
    VariantClear, VariantInit,
};
use windows::core::{BSTR, GUID, Interface, PCWSTR};

pub type DispResult<T> = Result<T, String>;

const LOCALE_USER_DEFAULT: u32 = 0x0400;
const DISPID_PROPERTYPUT: i32 = -3;
const IID_NULL: GUID = GUID::from_u128(0);

/// COM 初始化 RAII 守卫（等价 C# 的 [STAThread] + CLR 自动初始化）。
/// Word COM 自动化要求 STA；不初始化时 CoCreateInstance 会返回 CO_E_NOTINITIALIZED。
pub struct ComInit;

impl ComInit {
    pub fn sta() -> DispResult<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                .ok()
                .map_err(|_| "COM 初始化失败。".to_string())?;
        }
        Ok(ComInit)
    }
}

impl Drop for ComInit {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// IDispatch 包装。Drop 时由 windows-core Interface 语义自动 Release。
pub struct Disp(pub IDispatch);

/// 取 VARIANT 内部结构指针（绕开 ManuallyDrop 的 Deref 限制）。
#[inline]
unsafe fn inner_mut(x: &mut VARIANT) -> *mut VARIANT_0_0 {
    core::ptr::addr_of_mut!(x.Anonymous) as *mut VARIANT_0_0
}
#[inline]
unsafe fn inner(v: &VARIANT) -> *const VARIANT_0_0 {
    core::ptr::addr_of!(v.Anonymous) as *const VARIANT_0_0
}

impl Disp {
    /// 按 ProgID 创建 COM 实例（等价 C# 的 Activator.CreateInstance(Type.GetTypeFromProgID(...))）。
    pub fn from_progid(progid: PCWSTR) -> DispResult<Self> {
        unsafe {
            let clsid = CLSIDFromProgID(progid).map_err(|_| "Word COM 组件未注册。".to_string())?;
            let itf: IDispatch = CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER)
                .map_err(|_| "本机未安装 Word，或 Word COM 组件未注册。".to_string())?;
            Ok(Disp(itf))
        }
    }

    /// 解析成员 DISPID（等价 GetIDsOfNames）。
    unsafe fn dispid(&self, name: &str) -> DispResult<i32> {
        unsafe {
            let mut wide: Vec<u16> = name.encode_utf16().collect();
            wide.push(0);
            let names = [PCWSTR(wide.as_ptr())];
            let mut ids = [0i32];
            self.0
                .GetIDsOfNames(
                    &IID_NULL,
                    names.as_ptr(),
                    1,
                    LOCALE_USER_DEFAULT,
                    ids.as_mut_ptr(),
                )
                .map_err(|e| format!("未知成员「{name}」（{:#010x}）", e.code().0 as u32))?;
            Ok(ids[0])
        }
    }

    /// 统一 Invoke：args 按调用顺序传入（内部逆序放入 rgvarg）；named 为命名参数 DISPID 列表
    /// （与 args 一一对应：named[i] 对应调用顺序第 i 个参数）。
    unsafe fn invoke(
        &self,
        dispid: i32,
        flags: DISPATCH_FLAGS,
        mut args: Vec<VARIANT>,
        mut named: Vec<i32>,
    ) -> DispResult<VARIANT> {
        unsafe {
            args.reverse(); // rgvarg 按逆序排列
            named.reverse(); // rgdispidNamedArgs 与 rgvarg 位置一一对应（同为逆序）
            let params = DISPPARAMS {
                rgvarg: if args.is_empty() {
                    std::ptr::null_mut()
                } else {
                    args.as_mut_ptr()
                },
                rgdispidNamedArgs: if named.is_empty() {
                    std::ptr::null_mut()
                } else {
                    named.as_mut_ptr()
                },
                cArgs: args.len() as u32,
                cNamedArgs: named.len() as u32,
            };
            let mut result = VariantInit();
            let mut excep = EXCEPINFO::default();
            let mut argerr = 0u32;
            let hr = self.0.Invoke(
                dispid,
                &IID_NULL,
                LOCALE_USER_DEFAULT,
                flags,
                &params,
                Some(&mut result),
                Some(&mut excep),
                Some(&mut argerr),
            );
            // 无论成败，先释放参数 VARIANT（释放其中的 BSTR / 接口引用）
            for a in args.iter_mut() {
                let _ = VariantClear(a);
            }
            if let Err(e) = hr {
                let desc = bstr_to_string(&excep.bstrDescription);
                // EXCEPINFO 中的 BSTR 字段不会自动释放，手工接管防泄漏
                for f in [
                    &mut excep.bstrSource,
                    &mut excep.bstrDescription,
                    &mut excep.bstrHelpFile,
                ] {
                    drop(core::mem::ManuallyDrop::into_inner(core::mem::take(f)));
                }
                let msg = if desc.is_empty() {
                    format!("COM 错误 {:#010x}", e.code().0 as u32)
                } else {
                    desc
                };
                return Err(msg);
            }
            Ok(result)
        }
    }

    /// 属性读取（无参数）：等价 C# `obj.Name`。
    pub unsafe fn get(&self, name: &str) -> DispResult<VARIANT> {
        unsafe {
            let id = self.dispid(name)?;
            self.invoke(id, DISPATCH_PROPERTYGET, vec![], vec![])
        }
    }

    /// 属性写入：等价 C# `obj.Name = value`。
    pub unsafe fn put(&self, name: &str, val: VARIANT) -> DispResult<()> {
        unsafe {
            let id = self.dispid(name)?;
            let mut r = self.invoke(
                id,
                DISPATCH_PROPERTYPUT,
                vec![val],
                vec![DISPID_PROPERTYPUT],
            )?;
            let _ = VariantClear(&mut r);
            Ok(())
        }
    }

    /// 方法调用（位置参数，有返回值）：等价 C# `obj.Method(args...)`。
    pub unsafe fn call(&self, name: &str, args: Vec<VARIANT>) -> DispResult<VARIANT> {
        unsafe {
            let id = self.dispid(name)?;
            // METHOD|GET 组合标志与 C# DLR 对 Word 的调用方式一致
            self.invoke(id, DISPATCH_METHOD | DISPATCH_PROPERTYGET, args, vec![])
        }
    }

    /// 方法调用（位置参数，无返回值）。
    pub unsafe fn callv(&self, name: &str, args: Vec<VARIANT>) -> DispResult<()> {
        unsafe {
            let mut r = self.call(name, args)?;
            let _ = VariantClear(&mut r);
            Ok(())
        }
    }

    /// 方法调用（命名参数）：等价 C# `doc.PrintOut(Background: false, Pages: "1-3")`。
    /// named 按调用书写顺序给出 (参数名, 值)；值的所有权移入本调用（用后自动释放）。
    /// IDispatch 协议要求参数 DISPID 与成员名在同一次 GetIDsOfNames 中解析
    /// （Word 不支持脱离方法上下文单独解析参数名）。
    pub unsafe fn call_named(&self, name: &str, named: Vec<(&str, VARIANT)>) -> DispResult<()> {
        unsafe {
            let mut wide_names: Vec<Vec<u16>> = Vec::with_capacity(named.len() + 1);
            let mut ptrs: Vec<PCWSTR> = Vec::with_capacity(named.len() + 1);
            let push_name = |s: &str, wide_names: &mut Vec<Vec<u16>>, ptrs: &mut Vec<PCWSTR>| {
                let mut w: Vec<u16> = s.encode_utf16().collect();
                w.push(0);
                ptrs.push(PCWSTR(w.as_ptr()));
                wide_names.push(w);
            };
            push_name(name, &mut wide_names, &mut ptrs);
            let mut args: Vec<VARIANT> = Vec::with_capacity(named.len());
            for (n, v) in named {
                push_name(n, &mut wide_names, &mut ptrs);
                args.push(v);
            }
            let mut ids = vec![0i32; ptrs.len()];
            self.0
                .GetIDsOfNames(
                    &IID_NULL,
                    ptrs.as_ptr(),
                    ptrs.len() as u32,
                    LOCALE_USER_DEFAULT,
                    ids.as_mut_ptr(),
                )
                .map_err(|e| format!("未知成员「{name}」（{:#010x}）", e.code().0 as u32))?;
            let dispids = ids[1..].to_vec();
            let mut r = self.invoke(
                ids[0],
                DISPATCH_METHOD | DISPATCH_PROPERTYGET,
                args,
                dispids,
            )?;
            let _ = VariantClear(&mut r);
            Ok(())
        }
    }
}

fn bstr_to_string(b: &BSTR) -> String {
    unsafe { String::from_utf16_lossy(core::slice::from_raw_parts(b.as_ptr(), b.len())) }
}

// ==================== VARIANT 构造（等价 C# 隐式装箱） ====================

pub fn var_i4(v: i32) -> VARIANT {
    let mut x = unsafe { VariantInit() };
    unsafe {
        (*inner_mut(&mut x)).vt = VT_I4;
        (*inner_mut(&mut x)).Anonymous.lVal = v;
    }
    x
}

pub fn var_r4(v: f32) -> VARIANT {
    let mut x = unsafe { VariantInit() };
    unsafe {
        (*inner_mut(&mut x)).vt = VT_R4;
        (*inner_mut(&mut x)).Anonymous.fltVal = v;
    }
    x
}

pub fn var_bool(v: bool) -> VARIANT {
    let mut x = unsafe { VariantInit() };
    unsafe {
        (*inner_mut(&mut x)).vt = VT_BOOL;
        (*inner_mut(&mut x)).Anonymous.boolVal = VARIANT_BOOL(if v { -1 } else { 0 });
    }
    x
}

pub fn var_bstr(s: &str) -> VARIANT {
    let mut x = unsafe { VariantInit() };
    unsafe {
        (*inner_mut(&mut x)).vt = VT_BSTR;
        (*inner_mut(&mut x)).Anonymous.bstrVal = core::mem::ManuallyDrop::new(BSTR::from(s));
    }
    x
}

/// 把已有的 COM 对象包装成 VARIANT 以便作为参数传递（如 Tables.Add 的 Range 参数）。
/// AddRef 一次，最终由接收方/Invoke 后的 VariantClear 释放。
///
/// 目前这两个页面用不到；保留是为了让 `com.rs` 作为完整的 COM 助手层搬运过来，
/// 将来加新动作（例如读回文档名、按 Range 插表）不必再回头补。
#[allow(dead_code)]
pub fn var_disp(d: &Disp) -> VARIANT {
    let mut x = unsafe { VariantInit() };
    unsafe {
        (*inner_mut(&mut x)).vt = VT_DISPATCH;
        (*inner_mut(&mut x)).Anonymous.pdispVal = core::mem::ManuallyDrop::new(Some((d.0).clone()));
    }
    x
}

// ==================== VARIANT 读取（消费值并清理） ====================

pub unsafe fn res_i32(mut v: VARIANT) -> DispResult<i32> {
    unsafe {
        let r = match (*inner(&v)).vt {
            VT_I4 => (*inner(&v)).Anonymous.lVal,
            VT_I2 => (*inner(&v)).Anonymous.iVal as i32,
            VT_R4 => (*inner(&v)).Anonymous.fltVal as i32,
            VT_R8 => (*inner(&v)).Anonymous.dblVal as i32,
            VT_BOOL => {
                if (*inner(&v)).Anonymous.boolVal.0 != 0 {
                    -1
                } else {
                    0
                }
            }
            other => {
                let _ = VariantClear(&mut v);
                return Err(format!("预期整数，实际 VT={}", other.0));
            }
        };
        let _ = VariantClear(&mut v);
        Ok(r)
    }
}

pub unsafe fn res_f32(mut v: VARIANT) -> DispResult<f32> {
    unsafe {
        let r = match (*inner(&v)).vt {
            VT_R4 => (*inner(&v)).Anonymous.fltVal,
            VT_R8 => (*inner(&v)).Anonymous.dblVal as f32,
            VT_I4 => (*inner(&v)).Anonymous.lVal as f32,
            VT_I2 => (*inner(&v)).Anonymous.iVal as f32,
            other => {
                let _ = VariantClear(&mut v);
                return Err(format!("预期数值，实际 VT={}", other.0));
            }
        };
        let _ = VariantClear(&mut v);
        Ok(r)
    }
}

pub unsafe fn res_string(mut v: VARIANT) -> DispResult<String> {
    unsafe {
        let r = match (*inner(&v)).vt {
            VT_BSTR => bstr_to_string(&(*inner(&v)).Anonymous.bstrVal),
            other => {
                let _ = VariantClear(&mut v);
                return Err(format!("预期字符串，实际 VT={}", other.0));
            }
        };
        let _ = VariantClear(&mut v);
        Ok(r)
    }
}

pub unsafe fn res_disp(mut v: VARIANT) -> DispResult<Disp> {
    unsafe {
        let vt = (*inner(&v)).vt;
        let taken: Option<IDispatch> = match vt {
            VT_DISPATCH => (*(*inner(&v)).Anonymous.pdispVal).clone(),
            VT_UNKNOWN => {
                let u = (*(*inner(&v)).Anonymous.punkVal).clone();
                match u {
                    Some(u) => u.cast::<IDispatch>().ok(),
                    None => None,
                }
            }
            other => {
                let _ = VariantClear(&mut v);
                return Err(format!("预期 COM 对象，实际 VT={}", other.0));
            }
        };
        let _ = VariantClear(&mut v);
        taken.map(Disp).ok_or_else(|| "COM 对象为空".to_string())
    }
}
