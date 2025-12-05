#!/usr/bin/env python3
import os

import matplotlib.pyplot as plt
import pandas as pd

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
PROJECT_ROOT = os.path.dirname(SCRIPT_DIR)

INPUT_FILE = os.path.join(PROJECT_ROOT, "artefacts", "abba_metrics.csv")
OUTPUT_DIR = os.path.join(PROJECT_ROOT, "artefacts")


def load_data():
    if not os.path.exists(INPUT_FILE):
        print(f"Error: Missing {INPUT_FILE}. Run collect_metrics.sh first.")
        return None
    return pd.read_csv(INPUT_FILE)


def summarize(df):
    status_counts = df["Status"].value_counts()
    total = len(df)
    deadlocks = status_counts.get("DEADLOCK", 0)
    rate = (deadlocks / total) * 100 if total else 0

    print("-" * 40)
    print("       AB-BA DEADLOCK METRICS       ")
    print("-" * 40)
    print(f"Total Runs:              {total}")
    print(f"Successful Deadlocks:    {deadlocks}")
    print(f"Deadlock Success Rate:   {rate:.1f}%")
    print("\nStatus Breakdown:")
    print(status_counts.to_string())
    print("-" * 40)


def plot_status(df):
    counts = df["Status"].value_counts()

    plt.figure(figsize=(6, 4))
    plt.bar(counts.index, counts.values)
    plt.title(f"Race Condition Outcome (N={len(df)})")
    plt.ylabel("Count")
    plt.xlabel("Outcome")

    for i, v in enumerate(counts.values):
        plt.text(i, v, str(v), ha="center", va="bottom")

    out = f"{OUTPUT_DIR}/status_distribution.png"
    plt.savefig(out, bbox_inches="tight")
    print(f"[+] Saved plot: {out}")


def plot_blocked(df):
    counts = df["BlockedThreads"].value_counts().sort_index()

    plt.figure(figsize=(6, 4))
    plt.bar(counts.index.astype(str), counts.values)
    plt.title("Blocked Threads in D-State per Run")
    plt.xlabel("Number of Blocked Threads")
    plt.ylabel("Frequency")

    out = f"{OUTPUT_DIR}/blocked_threads.png"
    plt.savefig(out, bbox_inches="tight")
    print(f"[+] Saved plot: {out}")


def print_stats(df):
    print("\n[Summary Statistics]")
    print("-" * 20)
    stats = df[["BlockedThreads", "WaitQueue"]].describe().round(2)
    print(stats)

    try:
        print("\n[LaTeX Table Code]")
        print("-" * 20)
        print(stats.to_latex(float_format="%.2f"))
    except Exception:
        print("[!] Could not generate LaTeX code.")
    print("-" * 20)


def main():
    df = load_data()
    if df is None:
        return

    summarize(df)
    plot_status(df)
    plot_blocked(df)
    print_stats(df)


if __name__ == "__main__":
    main()
