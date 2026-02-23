import asyncio
from datetime import datetime
from typing import List, Optional

x = [1, 2, 3]
x.clear()



class SyntaxDemo:
    """
    A class to demonstrate various Python syntax features.
    """
    def __init__(self, name: str):
        self.name = name
        self.created_at = datetime.now()

    @property
    def info(self) -> str:
        return f"Name: {self.name}, Created: {self.created_at}"

    async def run_async_task(self, count: int) -> List[int]:
        print(f"Starting async task for {self.name}...")
        results = []
        for i in range(count):
            await asyncio.sleep(0.1)
            results.append(i * 2)
        return results

def complex_logic(items: Optional[List[int]] = None) -> int:
    if items is None:
        items = [x for x in range(10) if x % 2 == 0]
    
    match items:
        case []:
            return 0
        case [first, *rest]:
            return first + sum(rest)
        case _:
            return -1

async def main():
    demo = SyntaxDemo("Treesitter")
    print(demo.info)
    
    # List comprehension
    squares = [x**2 for x in range(5)]
    print(f"Squares: {squares}")
    
    # Async call
    vals = await demo.run_async_task(3)
    print(f"Results: {vals}")
    
    total = complex_logic(vals)
    print(f"Total: {total}")

if __name__ == "__main__":
    asyncio.run(main())
