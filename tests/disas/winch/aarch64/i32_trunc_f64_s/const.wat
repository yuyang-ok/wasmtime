;;! target = "aarch64"
;;! test = "winch"

(module
    (func (result i32)
        (f64.const 1.0)
        (i32.trunc_f64_s)
    )
)
;; wasm[0]::function[0]:
;;       stp     x29, x30, [sp, #-0x10]!
;;       mov     x29, sp
;;       str     x28, [sp, #-0x10]!
;;       mov     x28, sp
;;       ldur    x16, [x0, #8]
;;       ldur    x16, [x16, #0x10]
;;       mov     x17, #0
;;       movk    x17, #0x10
;;       add     x16, x16, x17
;;       cmp     sp, x16
;;       b.lo    #0x90
;;   2c: mov     x9, x0
;;       sub     x28, x28, #0x10
;;       mov     sp, x28
;;       stur    x0, [x28, #8]
;;       stur    x1, [x28]
;;       mov     x16, #0x3ff0000000000000
;;       fmov    d0, x16
;;       fcmp    d0, d0
;;       b.vs    #0x94
;;   50: mov     x16, #0x200000
;;       movk    x16, #0xc1e0, lsl #48
;;       fmov    d31, x16
;;       fcmp    d0, d31
;;       b.le    #0x98
;;   64: mov     x16, #0x41e0000000000000
;;       fmov    d31, x16
;;       fcmp    d0, d31
;;       b.ge    #0x9c
;;   74: fcvtzs  w0, d0
;;       add     x28, x28, #0x10
;;       mov     sp, x28
;;       mov     sp, x28
;;       ldr     x28, [sp], #0x10
;;       ldp     x29, x30, [sp], #0x10
;;       ret
;;   90: .byte   0x1f, 0xc1, 0x00, 0x00
;;   94: .byte   0x1f, 0xc1, 0x00, 0x00
;;   98: .byte   0x1f, 0xc1, 0x00, 0x00
;;   9c: .byte   0x1f, 0xc1, 0x00, 0x00
