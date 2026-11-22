// wasm-ld initializes passive segments once in shared memory. Binaryen may
// inline both the global relocations and the guard into the start function.
use Operator::*;
use wasmtime::wasmparser::{BlockType, FunctionBody, MemArg, Operator, ValType};

use crate::Layout;

fn operators<'a>(body: &FunctionBody<'a>) -> Option<Vec<Operator<'a>>> {
    body.get_operators_reader()
        .ok()?
        .into_iter()
        .collect::<Result<_, _>>()
        .ok()
}

pub(super) fn recognize<'a, 'b>(
    start: &'b FunctionBody<'a>,
    local_body: impl Fn(u32) -> Option<&'b FunctionBody<'a>>,
    bases: &[(u32, bool)],
    imported_globals: u32,
    globals: &[bool],
    layout: &Layout,
    passive_sizes: &[Option<usize>],
) -> bool
where
    'a: 'b,
{
    // Active data could overwrite the once flag before the start runs.
    if passive_sizes.iter().any(Option::is_none) {
        return false;
    }
    let Some(ops) = operators(start) else {
        return false;
    };
    let initializer = match ops.as_slice() {
        [
            Call {
                function_index: reloc,
            },
            Call {
                function_index: init,
            },
            End,
        ] => {
            let Some(relocs) = local_body(*reloc).and_then(operators) else {
                return false;
            };
            if !matches!(
                after_relocations(&relocs, bases, imported_globals, globals),
                [End]
            ) {
                return false;
            }
            local_body(*init)
        }
        [Call { function_index }, End] => local_body(*function_index),
        _ => {
            return guarded(
                start,
                after_relocations(&ops, bases, imported_globals, globals),
                bases,
                layout,
                passive_sizes,
            );
        }
    };
    initializer.is_some_and(|body| {
        operators(body).is_some_and(|ops| guarded(body, &ops, bases, layout, passive_sizes))
    })
}

// Recognize both operand orders produced for base + offset.
fn address<'s, 'a>(ops: &'s [Operator<'a>]) -> Option<(u32, u32, &'s [Operator<'a>])> {
    match ops {
        [
            GlobalGet { global_index },
            I32Const { value },
            I32Add,
            rest @ ..,
        ]
        | [
            I32Const { value },
            GlobalGet { global_index },
            I32Add,
            rest @ ..,
        ] => Some((*global_index, u32::try_from(*value).ok()?, rest)),
        // Binaryen may use subtraction of a negative offset to shorten its LEB encoding.
        [
            GlobalGet { global_index },
            I32Const { value },
            I32Sub,
            rest @ ..,
        ] => Some((*global_index, u32::try_from(-i64::from(*value)).ok()?, rest)),
        [GlobalGet { global_index }, rest @ ..] => Some((*global_index, 0, rest)),
        _ => None,
    }
}

fn after_relocations<'s, 'a>(
    mut ops: &'s [Operator<'a>],
    bases: &[(u32, bool)],
    imported: u32,
    globals: &[bool],
) -> &'s [Operator<'a>] {
    while let Some((base, _, [GlobalSet { global_index }, rest @ ..])) = address(ops) {
        if !bases.iter().any(|&(index, _)| index == base)
            || global_index
                .checked_sub(imported)
                .and_then(|i| globals.get(i as usize))
                != Some(&true)
        {
            break;
        }
        ops = rest;
    }
    ops
}

fn blocks<'s, 'a>(ops: &'s [Operator<'a>]) -> Option<&'s [Operator<'a>]> {
    match ops {
        [
            Block {
                blockty: BlockType::Empty,
            },
            Block {
                blockty: BlockType::Empty,
            },
            Block {
                blockty: BlockType::Empty,
            },
            rest @ ..,
        ] => Some(rest),
        _ => None,
    }
}

fn atomic_word(arg: &MemArg) -> bool {
    arg.memory == 0 && arg.offset == 0 && arg.align == 2
}

fn guarded(
    body: &FunctionBody<'_>,
    ops: &[Operator<'_>],
    bases: &[(u32, bool)],
    layout: &Layout,
    passive: &[Option<usize>],
) -> bool {
    let Some(&(memory_base, _)) = bases.iter().find(|(_, memory)| *memory) else {
        return false;
    };
    let Ok(locals) = body.get_locals_reader() else {
        return false;
    };
    if !matches!(
        locals.into_iter().collect::<Result<Vec<_>, _>>().as_deref(),
        Ok([(1, ValType::I32)])
    ) {
        return false;
    }
    let (base, guard, ops) = if let Some(inner) = blocks(ops) {
        let Some((base, guard, [LocalTee { local_index: 0 }, rest @ ..])) = address(inner) else {
            return false;
        };
        (base, guard, rest)
    } else {
        let Some((base, guard, [LocalSet { local_index: 0 }, rest @ ..])) = address(ops) else {
            return false;
        };
        let Some([LocalGet { local_index: 0 }, rest @ ..]) = blocks(rest) else {
            return false;
        };
        (base, guard, rest)
    };
    if base != memory_base
        || layout.memory_alignment < 2
        || guard % 4 != 0
        || guard
            .checked_add(4)
            .is_none_or(|end| end > layout.memory_size)
    {
        return false;
    }
    let [
        I32Const { value: 0 },
        I32Const { value: 1 },
        I32AtomicRmwCmpxchg { memarg },
        BrTable { targets },
        End,
        tail @ ..,
    ] = ops
    else {
        return false;
    };
    if !atomic_word(memarg)
        || targets.default() != 2
        || targets.len() != 2
        || !targets.targets().map(Result::ok).eq([Some(0), Some(1)])
    {
        return false;
    }
    let mut tail = tail;
    while let Some((base, destination, operands)) = address(tail) {
        let (offset, size, rest) = match operands {
            [
                I32Const { value: 0 },
                I32Const { value: size },
                write,
                rest @ ..,
            ] => {
                let Ok(size) = u32::try_from(*size) else {
                    return false;
                };
                match write {
                    MemoryInit { mem: 0, data_index }
                        if passive
                            .get(*data_index as usize)
                            .copied()
                            .flatten()
                            .is_some_and(|len| size as usize <= len) => {}
                    MemoryFill { mem: 0 } => {}
                    _ => return false,
                }
                (0, size, rest)
            }
            // Binaryen folds short BSS fills into a scalar store with a static offset.
            [I32Const { value: 0 }, I32Store { memarg }, rest @ ..] if memarg.memory == 0 => {
                (memarg.offset, 4, rest)
            }
            [I64Const { value: 0 }, I64Store { memarg }, rest @ ..] if memarg.memory == 0 => {
                (memarg.offset, 8, rest)
            }
            _ => return false,
        };
        let Some(start) = u64::from(destination).checked_add(offset) else {
            return false;
        };
        let Some(end) = start.checked_add(u64::from(size)) else {
            return false;
        };
        if base != memory_base
            || end > u64::from(layout.memory_size)
            || (size > 0 && start < u64::from(guard + 4) && end > u64::from(guard))
        {
            return false;
        }
        tail = rest;
    }
    let [
        LocalGet { local_index: 0 },
        I32Const { value: 2 },
        I32AtomicStore { memarg: store },
        LocalGet { local_index: 0 },
        I32Const { value: -1 },
        MemoryAtomicNotify { memarg: notify },
        Drop,
        Br { relative_depth: 1 },
        End,
        LocalGet { local_index: 0 },
        I32Const { value: 1 },
        I64Const { value: -1 },
        MemoryAtomicWait32 { memarg: wait },
        Drop,
        End,
        drops @ ..,
    ] = tail
    else {
        return false;
    };
    let Some((End, drops)) = drops.split_last() else {
        return false;
    };
    [store, notify, wait].into_iter().all(atomic_word) && drops.iter().all(|op| matches!(op, DataDrop {data_index} if passive.get(*data_index as usize).is_some_and(Option::is_some)))
}
