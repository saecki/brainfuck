use std::cmp::PartialOrd;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::ExitCode;

use crate::cli::{Command, Config, ANSII_CLEAR, ANSII_COLOR_YELLOW};

pub mod cli;
pub mod x86;

const NUM_REGISTERS: usize = 1 << 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Token {
    Shl,
    Shr,
    Inc,
    Dec,
    Output,
    Input,
    LSquare,
    RSquare,
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::Shl => write!(f, "<"),
            Token::Shr => write!(f, ">"),
            Token::Inc => write!(f, "+"),
            Token::Dec => write!(f, "-"),
            Token::Output => write!(f, "."),
            Token::Input => write!(f, ","),
            Token::LSquare => write!(f, "["),
            Token::RSquare => write!(f, "]"),
        }
    }
}

impl Token {
    pub fn is_combinable(self) -> bool {
        match self {
            Token::Shl | Token::Shr | Token::Inc | Token::Dec => true,
            Token::Output | Token::Input | Token::LSquare | Token::RSquare => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Instruction {
    Shl(u16),
    Shr(u16),
    Inc(u8),
    Dec(u8),
    Output,
    Input,
    /// Jump to the position if the current register value is zero.
    JumpZ(Jump),
    /// Jump to the position if the current register value is not zero.
    JumpNz(Jump),

    /// Clear the current register:
    /// ```bf
    /// [
    ///     -
    /// ]
    /// ```
    Zero(i16),
    /// Add current register value to register at offset.
    Add(i16),
    /// Subtract current register value from register at offset.
    Sub(i16),
    /// Multiply current register value and add to register at offset.
    AddMul(i16, u8),
    /// Multiply current register value and subtraction from register at offset.
    SubMul(i16, u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Jump {
    Location(NonZeroU32),
    Redundant,
}

impl Jump {
    pub fn is_redundant(&self) -> bool {
        matches!(self, Self::Redundant)
    }
}

impl std::fmt::Display for Instruction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Instruction::Shl(n) => write!(f, "< ({n})"),
            Instruction::Shr(n) => write!(f, "> ({n})"),
            Instruction::Inc(n) => write!(f, "+ ({n})"),
            Instruction::Dec(n) => write!(f, "- ({n})"),
            Instruction::Output => write!(f, "out"),
            Instruction::Input => write!(f, "in"),
            Instruction::JumpZ(_) => write!(f, "["),
            Instruction::JumpNz(_) => write!(f, "]"),

            Instruction::Zero(o) => write!(f, "<{o}> zero"),
            Instruction::Add(o) => write!(f, "<{o}> add"),
            Instruction::Sub(o) => write!(f, "<{o}> sub"),
            Instruction::AddMul(o, n) => write!(f, "<{o}> addmul({n})"),
            Instruction::SubMul(o, n) => write!(f, "<{o}> submul({n})"),
        }
    }
}

fn main() -> ExitCode {
    let (config, command, path) = match cli::parse_args() {
        ControlFlow::Continue(c) => c,
        ControlFlow::Break(e) => return e,
    };

    let input = std::fs::read_to_string(&path).unwrap();
    let bytes = input.as_bytes();

    let tokens = bytes
        .iter()
        .filter_map(|b| {
            let t = match *b {
                b'<' => Token::Shl,
                b'>' => Token::Shr,
                b'+' => Token::Inc,
                b'-' => Token::Dec,
                b'.' => Token::Output,
                b',' => Token::Input,
                b'[' => Token::LSquare,
                b']' => Token::RSquare,
                _ => return None,
            };
            Some(t)
        })
        .collect::<Vec<_>>();

    // combine instructions
    let mut instructions = tokens
        .chunk_by(|a, b| a.is_combinable() && a == b)
        .inspect(|c| {
            if config.verbose >= 2 && c.len() > 1 {
                println!("combine {}", c.len());
            }
        })
        .map(|chunk| match chunk[0] {
            Token::Shl => Instruction::Shl(chunk.len() as u16),
            Token::Shr => Instruction::Shr(chunk.len() as u16),
            Token::Inc => Instruction::Inc(chunk.len() as u8),
            Token::Dec => Instruction::Dec(chunk.len() as u8),
            Token::Output => Instruction::Output,
            Token::Input => Instruction::Input,
            Token::LSquare => Instruction::JumpZ(Jump::Location(NonZeroU32::MAX)),
            Token::RSquare => Instruction::JumpNz(Jump::Location(NonZeroU32::MAX)),
        })
        .collect::<Vec<_>>();
    if config.verbose >= 1 {
        println!("============================================================");
        println!(
            "tokens before {} after: {} ({:.3}%)",
            tokens.len(),
            instructions.len(),
            100.0 * instructions.len() as f32 / tokens.len() as f32,
        );
        println!("============================================================");
    }
    if config.verbose >= 3 || command == Command::Format {
        cli::print_brainfuck_code(&instructions);
        if command == Command::Format {
            return ExitCode::SUCCESS;
        }
    }

    if config.optimize {
        let prev_len = instructions.len();
        // zero register
        if config.o_zeros {
            use Instruction::*;

            let mut i = 0;
            while i + 3 < instructions.len() {
                let [a, b, c] = &instructions[i..i + 3] else {
                    unreachable!()
                };
                if let (JumpZ(_), Dec(1), JumpNz(_)) = (a, b, c) {
                    let range = i..i + 3;
                    if config.verbose >= 2 {
                        println!("replaced {range:?} with zero");
                    }
                    instructions.drain(range);
                    instructions.insert(i, Zero(0));
                }

                i += 1;
            }
        }
        // arithmetic instructions
        if config.o_arithmetic || config.o_jumps {
            let mut i = 0;
            while i < instructions.len() {
                arithmetic_loop_pass(&config, &mut instructions, i);
                i += 1;
            }
        }

        if config.o_dead_code {
            dead_code_elimination(&config, &mut instructions);
        }

        if config.verbose >= 1 {
            println!("============================================================");
            println!(
                "instructions before {} after: {} ({:.3}%)",
                prev_len,
                instructions.len(),
                100.0 * instructions.len() as f32 / prev_len as f32,
            );
            println!("============================================================");
        }
    }

    // update jump indices
    let mut par_stack = Vec::new();
    for (i, instruction) in instructions.iter_mut().enumerate() {
        match instruction {
            Instruction::JumpZ(closing_idx_ref) => par_stack.push((i, closing_idx_ref)),
            Instruction::JumpNz(opening_idx_ref) => {
                let Some((opening_idx, closing_idx_ref)) = par_stack.pop() else {
                    unreachable!("mismatched brackets")
                };

                if let Jump::Location(loc) = opening_idx_ref {
                    *loc = unsafe { NonZeroU32::new_unchecked(opening_idx as u32 + 1) };
                }
                if let Jump::Location(loc) = closing_idx_ref {
                    *loc = unsafe { NonZeroU32::new_unchecked(i as u32 + 1) };
                }
            }
            _ => (),
        }
    }
    if !par_stack.is_empty() {
        unreachable!("mismatched brackets")
    }

    if config.verbose >= 3 || command == Command::Ir {
        cli::print_instructions(&instructions);
        if command == Command::Ir {
            return ExitCode::SUCCESS;
        } else {
            println!("============================================================");
        }
    }

    match command {
        Command::Format => unreachable!(),
        Command::Ir => unreachable!(),
        Command::Run => run(&instructions),
        Command::Compile => {
            let code = x86::compile(&instructions);
            let path: &Path = path.as_ref();
            let bin_path = path.with_extension("elf");
            let mut file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .create(true)
                .mode(0o755)
                .open(bin_path)
                .unwrap();
            file.write(&code).unwrap();
        }
    }

    ExitCode::SUCCESS
}

fn run(instructions: &[Instruction]) {
    let mut ip = 0;
    let mut rp: usize = 0;
    let mut registers = [0u8; NUM_REGISTERS];
    loop {
        let Some(b) = instructions.get(ip) else {
            break;
        };

        match *b {
            Instruction::Shl(n) => rp -= n as usize,
            Instruction::Shr(n) => rp += n as usize,
            Instruction::Inc(n) => registers[rp] = registers[rp].wrapping_add(n),
            Instruction::Dec(n) => registers[rp] = registers[rp].wrapping_sub(n),
            Instruction::Output => {
                _ = std::io::stdout().write(&registers[rp..rp + 1]);
            }
            Instruction::Input => {
                _ = std::io::stdin().read(&mut registers[rp..rp + 1]);
            }
            Instruction::JumpZ(Jump::Location(idx)) => {
                if registers[rp] == 0 {
                    ip = idx.get() as usize;
                    continue;
                }
            }
            Instruction::JumpZ(Jump::Redundant) => (),
            Instruction::JumpNz(Jump::Location(idx)) => {
                if registers[rp] > 0 {
                    ip = idx.get() as usize;
                    continue;
                }
            }
            Instruction::JumpNz(Jump::Redundant) => (),

            Instruction::Zero(o) => registers[(rp as isize + o as isize) as usize] = 0,
            Instruction::Add(o) => {
                let val = registers[rp];
                let r = &mut registers[(rp as isize + o as isize) as usize];
                *r = r.wrapping_add(val);
            }
            Instruction::Sub(o) => {
                let val = registers[rp];
                let r = &mut registers[(rp as isize + o as isize) as usize];
                *r = r.wrapping_sub(val);
            }
            Instruction::AddMul(o, n) => {
                let val = n.wrapping_mul(registers[rp]);
                let r = &mut registers[(rp as isize + o as isize) as usize];
                *r = r.wrapping_add(val);
            }
            Instruction::SubMul(o, n) => {
                let val = n.wrapping_mul(registers[rp]);
                let r = &mut registers[(rp as isize + o as isize) as usize];
                *r = r.wrapping_sub(val);
            }
        }

        ip += 1;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IterationDiff {
    /// The change each loop iteration will have on the iteration register.
    Diff(i16),
    /// The loop always zeros the iteration register, this is equivalent to an if statement.
    Zeroed,
    /// The loop always zeros the iteration register, and then performs other operations on the
    /// iteration register. If the zeroed diff results in 0, this is also equivalent to an if
    /// statement, and the register is just used as temporary storage. Otherwise this is an
    /// infinite loop.
    ZeroedDiff(i16),
}

impl IterationDiff {
    fn inc(&mut self, inc: u8) {
        use IterationDiff::*;
        let inc = inc as i16;
        match self {
            Diff(d) | ZeroedDiff(d) => *d += inc,
            Zeroed => *self = ZeroedDiff(inc),
        }
    }

    fn dec(&mut self, dec: u8) {
        use IterationDiff::*;
        let dec = dec as i16;
        match self {
            Diff(d) | ZeroedDiff(d) => *d -= dec,
            Zeroed => *self = ZeroedDiff(-dec),
        }
    }

    fn zero(&mut self) {
        use IterationDiff::*;
        *self = Zeroed;
    }
}

fn arithmetic_loop_pass(config: &Config, instructions: &mut Vec<Instruction>, i: usize) {
    use Instruction::*;

    let JumpZ(_) = instructions[i] else { return };

    let start = i + 1;
    let mut end = None;
    for (j, inst) in instructions[start..].iter().enumerate() {
        match inst {
            JumpZ(_) => break,
            JumpNz(jump) => {
                end = Some((jump, start + j));
                break;
            }
            _ => (),
        }
    }
    let Some((end_jump, end)) = end else { return };
    let inner = &instructions[start..end];
    let mut offset = 0;
    let mut num_arith = 0;
    let mut iteration_diff = IterationDiff::Diff(0);
    for inst in inner {
        match inst {
            Shl(n) => offset -= *n as i16,
            Shr(n) => offset += *n as i16,
            Inc(n) => {
                if offset == 0 {
                    iteration_diff.inc(*n);
                } else {
                    num_arith += 1;
                }
            }
            Dec(n) => {
                if offset == 0 {
                    iteration_diff.dec(*n);
                } else {
                    num_arith += 1;
                }
            }
            Zero(o) => {
                if offset + o == 0 {
                    iteration_diff.zero();
                } else {
                    num_arith += 1;
                }
            }
            Output | Input | JumpZ(_) | JumpNz(_) | Add(_) | Sub(_) | AddMul(..) | SubMul(..) => {
                return
            }
        }
    }

    if offset != 0 {
        return;
    }

    match iteration_diff {
        IterationDiff::Diff(-1) => (),
        IterationDiff::Zeroed | IterationDiff::ZeroedDiff(0) => {
            if config.o_jumps {
                let JumpNz(jump) = &mut instructions[end] else {
                    unreachable!();
                };
                *jump = Jump::Redundant;
                if config.verbose >= 2 {
                    println!("redundant conditional jump at {}", end);
                }
            }
            return;
        }
        IterationDiff::Diff(0) | IterationDiff::ZeroedDiff(_) => {
            if !end_jump.is_redundant() {
                let range = start - 1..end + 1;
                let l = &instructions[range.clone()];
                eprintln!("{ANSII_COLOR_YELLOW}warning{ANSII_CLEAR}: infinite loop detected at {range:?}:\n{l:?}");
            }
            return;
        }
        IterationDiff::Diff(_) => return,
    }

    if !config.o_arithmetic {
        return;
    }

    let mut offset = 0;
    let mut replacements = Vec::with_capacity(num_arith + 1);
    for inst in inner.iter() {
        match inst {
            Shl(n) => offset -= *n as i16,
            Shr(n) => offset += *n as i16,
            Inc(n) => {
                if offset != 0 {
                    let replacement = match n {
                        1 => Add(offset),
                        _ => AddMul(offset, *n),
                    };
                    replacements.push(replacement);
                }
            }
            Dec(n) => {
                if offset != 0 {
                    let replacement = match n {
                        1 => Sub(offset),
                        _ => SubMul(offset, *n),
                    };
                    replacements.push(replacement);
                }
            }
            Zero(o) => {
                if offset + o != 0 {
                    replacements.extend([
                        JumpZ(Jump::Location(NonZeroU32::MAX)),
                        Zero(offset + o),
                        JumpNz(Jump::Redundant),
                    ]);
                }
            }
            Output | Input | JumpZ(_) | JumpNz(_) | Add(_) | Sub(_) | AddMul(..) | SubMul(..) => {
                unreachable!()
            }
        }
    }
    replacements.push(Zero(0));

    let range = start - 1..end + 1;
    if config.verbose >= 2 {
        println!("replaced {range:?} with {replacements:?}");
    }
    _ = instructions.splice(range, replacements);
}

fn dead_code_elimination(config: &Config, instructions: &mut Vec<Instruction>) {
    // execute instructions that are known to be constant time
    let mut registers = [0u8; NUM_REGISTERS];
    let mut rp = 0;
    let mut i = 0;
    while i < instructions.len() {
        let Some(inst) = instructions.get(i) else {
            unreachable!()
        };
        match inst {
            Instruction::Shl(n) => rp -= *n,
            Instruction::Shr(n) => rp += *n,
            Instruction::Inc(n) => {
                let reg = &mut registers[rp as usize];
                *reg = reg.wrapping_add(*n);
            }
            Instruction::Dec(n) => {
                let reg = &mut registers[rp as usize];
                *reg = reg.wrapping_sub(*n);
            }
            Instruction::Output => return,
            Instruction::Input => return,
            Instruction::JumpZ(_) => {
                let val = registers[rp as usize];
                if val != 0 {
                    return;
                }
                remove_dead_code(config, instructions, i);
                continue;
            }
            Instruction::JumpNz(_) => return,
            Instruction::Zero(o) => {
                let idx = rp as i16 + o;
                registers[idx as usize] = 0;
            }
            Instruction::Add(o) => {
                let val = registers[rp as usize];
                let idx = rp as i16 + o;
                let reg = &mut registers[idx as usize];
                *reg = reg.wrapping_add(val);
            }
            Instruction::Sub(o) => {
                let val = registers[rp as usize];
                let idx = rp as i16 + o;
                let reg = &mut registers[idx as usize];
                *reg = reg.wrapping_sub(val);
            }
            Instruction::AddMul(o, n) => {
                let val = registers[rp as usize];
                let idx = rp as i16 + o;
                let reg = &mut registers[idx as usize];
                *reg = reg.wrapping_add(val.wrapping_mul(*n));
            }
            Instruction::SubMul(o, n) => {
                let val = registers[rp as usize];
                let idx = rp as i16 + o;
                let reg = &mut registers[idx as usize];
                *reg = reg.wrapping_sub(val.wrapping_mul(*n));
            }
        }

        i += 1;
    }
}

fn remove_dead_code(config: &Config, instructions: &mut Vec<Instruction>, start: usize) {
    let mut jump_stack = 0;

    for (i, inst) in instructions[start..].iter().enumerate() {
        match inst {
            Instruction::JumpZ(_) => jump_stack += 1,
            Instruction::JumpNz(_) => {
                jump_stack -= 1;
                if jump_stack == 0 {
                    let range = start..start + i + 1;
                    if config.verbose >= 2 {
                        println!("removed dead code at {range:?}");
                    }
                    instructions.drain(range);
                    return;
                }
            }
            _ => (),
        }
    }

    unreachable!()
}
